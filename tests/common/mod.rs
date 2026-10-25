//! Test harness: runs the `minijob` binary on a copy of a configuration template.
#![allow(dead_code)]

use serde_json::Value;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

pub fn template_path(name: &str) -> PathBuf {
    Path::new(MANIFEST).join("config-templates").join(name)
}

/// A port no other server of this test binary got. Once the probe socket is closed the OS
/// may hand the port out again, also while a server is stopped for a restart: a second
/// server on it would fail to bind, and its harness would talk to (and stop) the first.
fn free_port() -> u16 {
    static TAKEN: std::sync::Mutex<std::collections::BTreeSet<u16>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if TAKEN.lock().unwrap().insert(port) {
            return port;
        }
    }
}

fn slash(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// A response: status, headers (lowercase names), body.
#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("invalid JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

pub struct Server {
    child: Option<Child>,
    pub base: String,
    pub port: u16,
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
    client: reqwest::blocking::Client,
}

pub fn client() -> reqwest::blocking::Client {
    minijob::tls::install_provider();
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Rewrites a template for the test: free port on 127.0.0.1, storage in the temp directory,
/// absolute schema path. `edit` can change anything else.
pub fn prepare_config(
    template: &str,
    dir: &Path,
    port: u16,
    edit: impl FnOnce(String) -> String,
) -> String {
    let text = std::fs::read_to_string(template_path(template)).unwrap();
    let schema_dir = template_path(template).parent().unwrap().to_path_buf();
    let data = dir.join("data");
    let text = text
        .replace("127.0.0.1:8080", &format!("127.0.0.1:{port}"))
        .replace("0.0.0.0:8080", &format!("127.0.0.1:{port}"))
        .replace("/data/minijob", &slash(&data))
        .replace("/app/data", &slash(&data))
        .replace(
            "./config-templates/standard/config/schema.json",
            &slash(&schema_dir.join("schema.json")),
        )
        .replace(
            "./config/schema.json",
            &slash(&schema_dir.join("schema.json")),
        )
        .replace(
            "filepath: \"events.log\"",
            &format!("filepath: \"{}\"", slash(&dir.join("events.log"))),
        );
    edit(text)
}

impl Server {
    pub fn start(template: &str) -> Server {
        Self::start_with(template, |t| t)
    }

    pub fn start_with(template: &str, edit: impl FnOnce(String) -> String) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let config = prepare_config(template, dir.path(), port, edit);
        Self::start_config(dir, port, &config)
    }

    /// Starts on an explicit configuration text (must listen on `port`).
    pub fn start_config(dir: tempfile::TempDir, port: u16, config: &str) -> Server {
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, config).unwrap();
        let log_path = dir.path().join("server.log");
        let mut s = Server {
            child: None,
            base: format!("http://127.0.0.1:{port}"),
            port,
            dir,
            config_path,
            log_path,
            client: client(),
        };
        s.spawn();
        s
    }

    /// (Re)starts the process on the same configuration and directory.
    pub fn spawn(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_minijob"))
            .arg("-config")
            .arg(&self.config_path)
            .current_dir(MANIFEST)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.child = Some(child);
        self.wait_ready();
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(r) = self.client.get(format!("{}/ping", self.base)).send()
                && r.status() == 200
            {
                return;
            }
            if let Some(status) = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
            {
                panic!("server exited with {status}: {}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server not ready: {}", self.log());
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn send(&self, req: reqwest::blocking::RequestBuilder) -> Resp {
        let r = req
            .send()
            .unwrap_or_else(|e| panic!("{e:?}\nserver log:\n{}", self.log()));
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let body = r.bytes().unwrap().to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    pub fn get(&self, path: &str) -> Resp {
        self.send(self.client.get(self.url(path)))
    }

    pub fn head(&self, path: &str) -> Resp {
        self.send(self.client.head(self.url(path)))
    }

    pub fn delete(&self, path: &str) -> Resp {
        self.send(self.client.delete(self.url(path)))
    }

    pub fn post(&self, path: &str) -> Resp {
        self.send(self.client.post(self.url(path)))
    }

    pub fn post_json(&self, path: &str, body: &str) -> Resp {
        self.send(
            self.client
                .post(self.url(path))
                .header("Content-Type", "application/json")
                .body(body.to_string()),
        )
    }

    pub fn request(&self, method: &str, path: &str) -> reqwest::blocking::RequestBuilder {
        self.client.request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            self.url(path),
        )
    }

    /// Creates a job, returns its id.
    pub fn create(&self, body: &str) -> String {
        let r = self.post_json("/api/v1/job/", body);
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["job"]["id"].as_str().unwrap().to_string()
    }

    pub fn job(&self, id: &str) -> Value {
        let r = self.get(&format!("/api/v1/job/{id}"));
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["job"].clone()
    }

    pub fn state(&self, id: &str) -> String {
        self.job(id)["state"].as_str().unwrap().to_string()
    }

    /// Waits until the job reaches `state` (the scheduler ticks every second).
    pub fn wait_state(&self, id: &str, state: &str, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let s = self.state(id);
            if s == state {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} is {s}, expected {state}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Pulls jobs; returns the ids (empty on 404).
    pub fn pull(&self, query: &str) -> Vec<String> {
        let r = self.post(&format!("/api/v1/job/pull?{query}"));
        if r.status == 404 {
            return Vec::new();
        }
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// Stops through the admin API and returns the exit status.
    pub fn shutdown(&mut self) -> ExitStatus {
        let r = self.post("/api/v1/admin/server/shutdown");
        assert_eq!(r.status, 200);
        self.wait_exit()
    }

    pub fn wait_exit(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("running");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(s) = child.try_wait().unwrap() {
                return s;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("server did not stop: {}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills the process without any shutdown (crash).
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Server {
    /// Stops the server through the admin API, so that it exits normally (and writes its
    /// coverage data under `cargo llvm-cov`); kills it when it does not answer.
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let url = self.url("/api/v1/admin/server/shutdown");
        let stopped = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .ok()
            .and_then(|c| c.post(url).send().ok())
            .is_some_and(|r| r.status() == 200);
        if stopped && let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    self.child = None;
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.kill();
    }
}

/// Runs the binary with arguments and returns (status, stdout, stderr).
pub fn run_binary(args: &[&str]) -> (ExitStatus, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_minijob"))
        .args(args)
        .current_dir(MANIFEST)
        .output()
        .unwrap();
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

impl Server {
    /// Process id of the running server.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }
}
