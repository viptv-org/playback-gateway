//! Private supervised stdio for the shared runtime. No source or reply is log data.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 6 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkerError {
    Unavailable,
    Refused,
    Protocol,
    Timeout,
    Unsettled,
}
impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "torrent_worker_unavailable",
            Self::Refused => "torrent_worker_refused",
            Self::Protocol => "torrent_worker_protocol_invalid",
            Self::Timeout => "torrent_worker_timeout",
            Self::Unsettled => "torrent_worker_unsettled",
        })
    }
}
impl std::error::Error for WorkerError {}
type Result<T> = std::result::Result<T, WorkerError>;
struct Request {
    bytes: Vec<u8>,
    response: mpsc::SyncSender<Result<Value>>,
}

pub struct Worker {
    requests: mpsc::SyncSender<Request>,
    child: Arc<Mutex<Child>>,
}
impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentWorker(<redacted>)")
    }
}
impl Worker {
    pub fn start(executable: &Path, cache: &Path, capacity: u64) -> Result<Self> {
        let mut command = Command::new(executable);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("API_KEY")
            .env("GOMAXPROCS", "2")
            .spawn()
            .map_err(|_| WorkerError::Unavailable)?;
        let mut input = child.stdin.take().ok_or(WorkerError::Unavailable)?;
        let output = child.stdout.take().ok_or(WorkerError::Unavailable)?;
        let child = Arc::new(Mutex::new(child));
        let (requests, pending) = mpsc::sync_channel::<Request>(8);
        std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            let mut initialized = false;
            for request in pending {
                let response = (|| {
                    if !initialized {
                        let startup: Value = serde_json::from_slice(&read_frame(&mut output)?)
                            .map_err(|_| WorkerError::Protocol)?;
                        if startup["version"].as_u64() != Some(2)
                            || startup["ready"] != true
                            || startup.get("ok").is_some()
                        {
                            return Err(WorkerError::Protocol);
                        }
                        initialized = true;
                    }
                    input
                        .write_all(&request.bytes)
                        .map_err(|_| WorkerError::Unavailable)?;
                    input
                        .write_all(b"\n")
                        .map_err(|_| WorkerError::Unavailable)?;
                    input.flush().map_err(|_| WorkerError::Unavailable)?;
                    let bytes = read_frame(&mut output)?;
                    let value: Value =
                        serde_json::from_slice(&bytes).map_err(|_| WorkerError::Protocol)?;
                    if value["version"].as_u64() != Some(2) || !value["ok"].is_boolean() {
                        return Err(WorkerError::Protocol);
                    }
                    Ok(value)
                })();
                let broken = response.is_err();
                let _ = request.response.send(response);
                if broken {
                    break;
                }
            }
        });
        let owner = Self { requests, child };
        let response = owner.call(json!({"op":"open","config":{"cache_dir":cache,"cache_bytes":capacity,"readahead_bytes":32<<20,"max_inputs":8,"max_handles":64}}), Duration::from_secs(8))?;
        if response["ok"] != true {
            return Err(WorkerError::Refused);
        }
        Ok(owner)
    }
    /// A caller timeout kills this owned child, including a blocked stdin writer.
    pub fn call(&self, mut command: Value, budget: Duration) -> Result<Value> {
        if !command.is_object() || !self.is_alive() {
            return Err(WorkerError::Unavailable);
        }
        command["version"] = json!(2);
        let bytes = serde_json::to_vec(&command).map_err(|_| WorkerError::Protocol)?;
        if bytes.len() > MAX_FRAME {
            return Err(WorkerError::Protocol);
        }
        let (response, receive) = mpsc::sync_channel(1);
        self.requests
            .try_send(Request { bytes, response })
            .map_err(|_| WorkerError::Unavailable)?;
        let result = receive
            .recv_timeout(budget)
            .map_err(|_| WorkerError::Timeout)
            .and_then(|value| value);
        if result.as_ref().is_err()
            || result
                .as_ref()
                .is_ok_and(|value| value["terminate"] == true)
        {
            self.terminate()?;
        }
        result
    }
    pub fn is_alive(&self) -> bool {
        self.child
            .lock()
            .ok()
            .is_some_and(|mut child| matches!(child.try_wait(), Ok(None)))
    }
    pub fn terminate(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        {
            let mut child = self.child.lock().map_err(|_| WorkerError::Unsettled)?;
            if child
                .try_wait()
                .map_err(|_| WorkerError::Unsettled)?
                .is_some()
            {
                return Ok(());
            }
            child.kill().map_err(|_| WorkerError::Unsettled)?;
        }
        loop {
            if self
                .child
                .lock()
                .map_err(|_| WorkerError::Unsettled)?
                .try_wait()
                .map_err(|_| WorkerError::Unsettled)?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(WorkerError::Unsettled);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn shutdown(&self) -> Result<()> {
        let _ = self.call(json!({"op":"shutdown"}), Duration::from_millis(1750));
        self.terminate()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
fn read_frame(input: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = input.fill_buf().map_err(|_| WorkerError::Unavailable)?;
        if chunk.is_empty() {
            return Err(WorkerError::Unavailable);
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let size = end.unwrap_or(chunk.len());
        if size > MAX_FRAME - bytes.len() {
            return Err(WorkerError::Protocol);
        }
        bytes.extend_from_slice(&chunk[..size]);
        input.consume(size + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_limit_is_enforced_before_an_unbounded_line_allocation() {
        assert_eq!(
            read_frame(&mut &b"{\"ok\":true}\nrest"[..]).unwrap(),
            b"{\"ok\":true}"
        );
        assert_eq!(
            read_frame(&mut vec![b'a'; MAX_FRAME + 1].as_slice()).unwrap_err(),
            WorkerError::Protocol
        );
    }
    #[cfg(unix)]
    #[test]
    fn readiness_banner_precedes_the_first_control_reply() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("viptv-banner-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("worker");
        std::fs::write(&script, b"#!/bin/sh\nprintf '%s\\n' '{\"version\":2,\"ready\":true,\"qualification\":\"candidate\"}'\nwhile IFS= read -r request; do printf '%s\\n' '{\"version\":2,\"ok\":true}'; done\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = Worker::start(&script, &root, 64 << 20).unwrap();
        assert_eq!(
            worker
                .call(json!({"op":"observe","handle":"1"}), Duration::from_secs(1))
                .unwrap()["ok"],
            true
        );
        worker.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn hung_owned_child_is_killed_and_reaped() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("viptv-worker-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("worker");
        std::fs::write(&script, b"#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        assert_eq!(
            Worker::start(&script, &root, 64 << 20).unwrap_err(),
            WorkerError::Timeout
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(root).unwrap();
    }
}
