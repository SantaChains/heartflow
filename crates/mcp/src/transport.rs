use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Byte stream to an MCP peer, framed as newline-delimited JSON.
pub trait Transport: Send {
    fn send_line(&mut self, line: &str) -> io::Result<()>;

    /// Wait up to `timeout` for the next line. `Ok(None)` means the peer
    /// closed the stream.
    fn recv_line(&mut self, timeout: Duration) -> io::Result<Option<String>>;
}

/// Transport over a spawned server process: JSON-RPC lines in, JSON-RPC lines
/// out. A reader thread drains stdout so responses can carry a deadline.
pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<io::Result<String>>,
}

impl StdioTransport {
    /// Spawn `program` with inherited environment plus `env` additions.
    pub fn spawn(
        program: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Server diagnostics must never reach our stdout/stderr.
            .stderr(Stdio::null());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "mcp server stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "mcp server stdout unavailable")
        })?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                        if tx.send(Ok(trimmed)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            lines: rx,
        })
    }
}

impl Transport for StdioTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    fn recv_line(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        match self.lines.recv_timeout(timeout) {
            Ok(result) => result.map(Some),
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "mcp server response timed out",
            )),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
pub(crate) struct ScriptedTransport {
    sent: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    incoming: std::collections::VecDeque<String>,
}

#[cfg(test)]
impl ScriptedTransport {
    /// Returns the transport plus a live handle to everything sent through it.
    pub(crate) fn new(
        incoming: Vec<String>,
    ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                sent: std::sync::Arc::clone(&sent),
                incoming: incoming.into(),
            },
            sent,
        )
    }
}

#[cfg(test)]
impl Transport for ScriptedTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.sent
            .lock()
            .expect("sent log lock")
            .push(line.to_string());
        Ok(())
    }

    fn recv_line(&mut self, _timeout: Duration) -> io::Result<Option<String>> {
        Ok(self.incoming.pop_front())
    }
}
