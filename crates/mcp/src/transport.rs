use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

/// Upper bound on buffered server lines. When full, the reader thread blocks,
/// the stdout pipe fills, and the server's writes block in turn: backpressure
/// reaches the server instead of growing our memory without bound. Bounded by
/// line count, not bytes, but each line is a single JSON-RPC frame and a
/// server that outpaces us by this many responses is already misbehaving.
const READER_CHANNEL_LINES: usize = 1024;
/// Ceiling for one stdin write. Matches the response timeout's spirit: a
/// server that stops reading its stdin (deadlock) must surface as an error
/// here instead of hanging the calling thread forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

enum WriteOutcome {
    Done,
    Failed(io::Error),
}

/// One queued stdin write plus the one-shot ack it must signal.
type WriteCommand = (String, SyncSender<WriteOutcome>);

/// Byte stream to an MCP peer, framed as newline-delimited JSON.
pub trait Transport: Send {
    fn send_line(&mut self, line: &str) -> io::Result<()>;

    /// Wait up to `timeout` for the next line. `Ok(None)` means the peer
    /// closed the stream.
    fn recv_line(&mut self, timeout: Duration) -> io::Result<Option<String>>;
}

/// Transport over a spawned server process: JSON-RPC lines in, JSON-RPC lines
/// out. A reader thread drains stdout so responses can carry a deadline; a
/// writer thread owns stdin so a server that stops reading surfaces as a
/// write timeout rather than a permanently hung caller.
pub struct StdioTransport {
    child: Child,
    writer: mpsc::Sender<WriteCommand>,
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

        let (tx, rx) = mpsc::sync_channel(READER_CHANNEL_LINES);
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

        let (write_tx, write_rx): (mpsc::Sender<WriteCommand>, mpsc::Receiver<WriteCommand>) =
            mpsc::channel();
        thread::spawn(move || {
            let mut stdin = stdin;
            for (line, ack) in write_rx {
                let outcome = (|| {
                    stdin.write_all(line.as_bytes())?;
                    stdin.write_all(b"\n")?;
                    stdin.flush()
                })();
                let outcome = match outcome {
                    Ok(()) => WriteOutcome::Done,
                    Err(error) => WriteOutcome::Failed(error),
                };
                if ack.send(outcome).is_err() {
                    break;
                }
            }
            // Dropping stdin here signals EOF to the server.
        });

        Ok(Self {
            child,
            writer: write_tx,
            lines: rx,
        })
    }
}

impl Transport for StdioTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.writer
            .send((line.to_string(), ack_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mcp server stdin closed"))?;
        match ack_rx.recv_timeout(WRITE_TIMEOUT) {
            Ok(WriteOutcome::Done) => Ok(()),
            Ok(WriteOutcome::Failed(error)) => Err(error),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mcp server stdin writer stopped",
            )),
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "mcp server stopped reading stdin",
            )),
        }
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
