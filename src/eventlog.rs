//! Event log file (`eventsLogger`), one JSON object per line, appended
//! (`eventlogger/eventloggerobserver.go`).
//!
//! Lines are written by a dedicated thread through a buffered writer that is flushed as soon
//! as no event is waiting, so a burst costs one write per batch instead of one per event.
//! Unlike Go, a quote, backslash or control character in the topic is escaped (Go wrote
//! the topic as is, breaking the line), and a write error is logged instead of crashing
//! the process.

use crate::events::ServiceEvent;
use crate::log::{self, Logger, gotime};
use crate::model::Job;
use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::mpsc;
use std::thread::JoinHandle;

const TIMESTAMP_LAYOUT: &str = "2006-01-02T15:04:05.000";

pub struct EventLog {
    tx: Mutex<Option<mpsc::Sender<String>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn timestamp() -> String {
    gotime::format(&gotime::now_local(), TIMESTAMP_LAYOUT)
}

/// Go wrote the topic as is. Only what would break the JSON line is escaped (quote,
/// backslash, control characters); everything else, `<`, `&` or U+2028 included, stays raw.
fn escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("{}u{:04x}", '\\', c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl EventLog {
    /// Creates the file if needed, otherwise opens it for appending (`OpenOrCreateFile`).
    pub fn open(path: &str, logger: Logger) -> Result<EventLog, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| crate::config::go_open_error(path, &e))?;
        let (tx, rx) = mpsc::channel::<String>();
        let thread = std::thread::Builder::new()
            .name("eventlog".into())
            .spawn(move || {
                let mut w = BufWriter::with_capacity(64 * 1024, file);
                while let Ok(line) = rx.recv() {
                    let mut result = w.write_all(line.as_bytes());
                    while let Ok(more) = rx.try_recv() {
                        if result.is_ok() {
                            result = w.write_all(more.as_bytes());
                        }
                    }
                    if let Err(e) = result.and_then(|_| w.flush()) {
                        logger.error(
                            "Cannot write to the events log file",
                            &[log::s("topic", "eventsLogger"), log::err(e)],
                        );
                    }
                }
                let _ = w.flush();
            })
            .map_err(|e| e.to_string())?;
        Ok(EventLog {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        })
    }

    fn send(&self, line: String) {
        if let Some(tx) = self.tx.lock().as_ref() {
            let _ = tx.send(line);
        }
    }

    fn filtered(ev: ServiceEvent) -> bool {
        matches!(
            ev,
            ServiceEvent::EmptyPollReceived
                | ServiceEvent::JobHealthcheck
                | ServiceEvent::JobMetrics
        )
    }

    /// Service-level event (`NotifyEvent`, and `NotifyTopicEvent`, which ignores the topic).
    pub fn service_event(&self, ev: ServiceEvent) {
        if Self::filtered(ev) {
            return;
        }
        self.send(format!(
            "{{\"timestamp\": \"{}\", \"type\": \"{}\"}}\n",
            timestamp(),
            ev.name()
        ));
    }

    /// Job event (`NotifyJobEvent`), never filtered.
    pub fn job_event(&self, ev: ServiceEvent, job: &Job) {
        self.send(format!(
            "{{\"timestamp\": \"{}\", \"type\": \"{}\", \"jobUUID\": \"{}\", \"topic\": \"{}\"}}\n",
            timestamp(),
            ev.name(),
            job.id,
            escaped(&job.topic)
        ));
    }

    /// Flushes and closes the file.
    pub fn close(&self) {
        self.tx.lock().take();
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_like_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.log");
        std::fs::write(&path, "previous\n").unwrap();
        let log = EventLog::open(path.to_str().unwrap(), Logger::nop()).unwrap();
        log.service_event(ServiceEvent::Ready);
        log.service_event(ServiceEvent::EmptyPollReceived);
        let mut j = Job {
            topic: "a\"b<&>\u{2028}\n".into(),
            ..Default::default()
        };
        j.init(uuid::Uuid::nil(), 1);
        log.job_event(ServiceEvent::JobHidden, &j);
        log.close();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "previous");
        assert!(lines[1].starts_with("{\"timestamp\": \""));
        assert!(lines[1].ends_with("\", \"type\": \"Ready\"}"));
        assert!(lines[2].ends_with(
            "\"type\": \"Hidden\", \"jobUUID\": \"00000000-0000-0000-0000-000000000000\", \"topic\": \"a\\\"b<&>\u{2028}\\n\"}"
        ));
        for l in &lines[1..] {
            serde_json::from_str::<serde_json::Value>(l).unwrap();
        }
    }
}
