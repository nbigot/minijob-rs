//! The in-memory backend file: `{"jobs":{"<uuid>":<job>,...}}`, same bytes as Go
//! (`jobbackendprovider/inmemoryprovider/dbstorage.go`).
//!
//! Unlike Go, the file is replaced atomically (temporary file, fsync, rename), so a crash
//! during a save cannot leave a truncated file.

use crate::gojson::decode::{Entries, match_field};
use crate::log::{self, Logger};
use crate::model::{Job, serialize_uuid_map};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserializer, Serialize, Serializer};
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

pub struct JobsFile {
    directory: PathBuf,
    path: PathBuf,
    logger: Logger,
}

struct JobsMap<'a>(&'a [Arc<Job>]);

impl Serialize for JobsMap<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_uuid_map(s, self.0.iter().map(|j| (&j.id, &**j)))
    }
}

#[derive(Serialize)]
struct FileStruct<'a> {
    jobs: JobsMap<'a>,
}

/// Encodes the file content.
pub fn encode(jobs: &[Arc<Job>]) -> Vec<u8> {
    crate::gojson::to_vec(&FileStruct {
        jobs: JobsMap(jobs),
    })
}

/// An object kept as members in order (Go decodes duplicates in order).
struct Ordered(Option<Entries>);

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a job object")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Ordered, E> {
                Ok(Ordered(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<Ordered, A::Error> {
                crate::gojson::decode::raw_members(m).map(|e| Ordered(Some(e)))
            }
        }
        d.deserialize_any(V)
    }
}

struct JobsSeed<'a>(&'a mut Vec<Job>);

impl<'de> DeserializeSeed<'de> for JobsSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        struct V<'a>(&'a mut Vec<Job>);
        impl<'de> Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of jobs")
            }
            fn visit_unit<E: de::Error>(self) -> Result<(), E> {
                Ok(())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
                while let Some(key) = m.next_key::<String>()? {
                    let id = crate::model::uuid::parse_go(&key).map_err(de::Error::custom)?;
                    let Ordered(entries) = m.next_value()?;
                    // A null value is a nil *Job in Go (it would crash later); skip it.
                    let Some(entries) = entries else { continue };
                    let mut job = Job::from_entries(&entries).map_err(de::Error::custom)?;
                    if job.id.is_nil() {
                        job.id = id;
                    }
                    self.0.push(job);
                }
                Ok(())
            }
        }
        d.deserialize_any(V(self.0))
    }
}

struct FileVisitor<'a>(&'a mut Vec<Job>);

impl<'de> Visitor<'de> for FileVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object with a \"jobs\" member")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        while let Some(key) = m.next_key::<String>()? {
            if match_field(&key, &["jobs"]).is_some() {
                m.next_value_seed(JobsSeed(&mut *self.0))?;
            } else {
                m.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

/// Decodes the file content. Like Go's `json.Decoder`, data after the first value is ignored.
pub fn decode(bytes: &[u8]) -> Result<Vec<Job>, String> {
    let text = crate::gojson::decode::go_text(bytes);
    let mut jobs = Vec::new();
    let mut de = serde_json::Deserializer::from_str(&text);
    (&mut de)
        .deserialize_any(FileVisitor(&mut jobs))
        .map_err(|e| e.to_string())?;
    Ok(jobs)
}

impl JobsFile {
    pub fn new(directory: &str, filename: &str, logger: Logger) -> JobsFile {
        let directory = PathBuf::from(directory);
        let path = directory.join(filename);
        JobsFile {
            directory,
            path,
            logger,
        }
    }

    /// Creates the directory and an empty file if needed, then reads the jobs.
    pub async fn load(&self) -> Result<Vec<Job>, String> {
        let directory = self.directory.clone();
        let path = self.path.clone();
        let logger = self.logger.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<Job>, String> {
            if !directory.as_os_str().is_empty() && !directory.exists() {
                std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            }
            if !path.exists() {
                write_atomic(&path, &encode(&[]))?;
                return Ok(Vec::new());
            }
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            decode(&bytes).inspect_err(|e| {
                logger.error(
                    "Can't decode json stream",
                    &[
                        log::s("topic", "stream"),
                        log::s("method", "LoadStreamFromUUID"),
                        log::s("filename", path.display().to_string()),
                        log::err(e),
                    ],
                );
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Writes all jobs (serialization and I/O off the async threads).
    pub async fn save(&self, jobs: Vec<Arc<Job>>) -> Result<(), String> {
        let path = self.path.clone();
        let logger = self.logger.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = encode(&jobs);
            write_atomic(&path, &bytes).inspect_err(|e| {
                logger.error(
                    "Can't save json to file",
                    &[
                        log::s("topic", "DBFileStorage"),
                        log::s("method", "Save"),
                        log::s("filename", path.display().to_string()),
                        log::err(e),
                    ],
                );
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    pub fn disk_usage(&self) -> u64 {
        match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(e) => {
                self.logger.error(
                    "Can't get file info",
                    &[
                        log::s("topic", "DBFileStorage"),
                        log::s("method", "GetDiskUsage"),
                        log::s("filename", self.path.display().to_string()),
                        log::err(e),
                    ],
                );
                0
            }
        }
    }
}

/// Replaces `path` with `bytes`: temporary file in the same directory, fsync, rename.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{JobState, events};

    #[test]
    fn roundtrip_and_go_layout() {
        let mut a = Job {
            topic: "t".into(),
            ..Default::default()
        };
        a.init(
            uuid::Uuid::parse_str("01a14533-a547-7cb9-a8c3-75493ba4d555").unwrap(),
            5,
        );
        a.add_event(events::PENDING, 6);
        let mut b = Job::default();
        b.init(
            uuid::Uuid::parse_str("00000000-0000-7000-8000-000000000001").unwrap(),
            1,
        );
        let bytes = encode(&[Arc::new(a.clone()), Arc::new(b.clone())]);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("{\"jobs\":{\"00000000-0000-7000-8000-000000000001\":{\"id\":"));
        let jobs = decode(&bytes).unwrap();
        assert_eq!(jobs.len(), 2);
        let back = jobs.iter().find(|j| j.id == a.id).unwrap();
        assert_eq!(back, &a);
        assert_eq!(back.state(), JobState::Pending);
        assert_eq!(encode(&[]), b"{\"jobs\":{}}");
    }

    #[test]
    fn reads_go_written_file() {
        // Written by the Go server (in-memory backend, one pending job).
        let go = br#"{"jobs":{"01a14533-a547-7cb9-a8c3-75493ba4d555":{"id":"01a14533-a547-7cb9-a8c3-75493ba4d555","topic":"t","priority":0,"properties":{"a":1,"b":12345678901234567000,"c":1e-7},"history":[{"eventType":"CREATE","timestamp":1792162374983},{"eventType":"PENDING","timestamp":1792162374983}],"lockResources":["r1"],"userAgent":"","requester":"","name":"","sessionId":"","traceId":"","visibilityTimeout":0,"startAfter":0,"callbackUrl":""}}} trailing"#;
        let jobs = decode(go).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].resources(), ["r1"]);
        assert_eq!(jobs[0].state(), JobState::Pending);
        assert_eq!(
            String::from_utf8(encode(&[Arc::new(jobs[0].clone())])).unwrap(),
            std::str::from_utf8(&go[..go.len() - " trailing".len()]).unwrap()
        );
        assert!(decode(b"{\"jobs\":{\"not-a-uuid\":{}}}").is_err());
        assert!(decode(b"{\"jobs\":null}").unwrap().is_empty());
    }

    #[test]
    fn atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("jobs.json");
        std::fs::write(&p, "x".repeat(1000)).unwrap();
        write_atomic(&p, b"{\"jobs\":{}}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"jobs\":{}}");
        assert!(!dir.path().join("jobs.json.tmp").exists());
    }
}
