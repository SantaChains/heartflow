use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::runtime::Runtime;

use crate::transport::Transport;

/// Upper bound for one HTTP round trip; remote servers must stay responsive.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// A request to POST one JSON-RPC message; the worker replies with every
/// inbound line the response produced (one for `application/json`, possibly
/// many for an `text/event-stream` body).
struct Job {
    body: String,
    reply: Sender<io::Result<Vec<String>>>,
}

/// Streamable-HTTP MCP transport.
///
/// Each JSON-RPC request is `POST`ed to a single endpoint with
/// `Accept: application/json, text/event-stream`; the reply is either one JSON
/// object or an SSE body whose `data:` frames carry the JSON-RPC messages. The
/// `Mcp-Session-Id` returned by `initialize` is replayed on later requests.
///
/// A dispatcher thread owns an async `reqwest` client and its own tokio
/// runtime shared with per-request worker threads, so `send_line`/`recv_line`
/// stay synchronous for callers exactly like
/// [`StdioTransport`](crate::StdioTransport) — and never trip the
/// `block_on` inside a runtime panic regardless of the calling context.
/// Concurrent tool calls therefore POST in parallel instead of queueing on a
/// single worker.
///
/// Limitation (documented, not a defect): the optional server-initiated GET SSE
/// channel is not opened. heartflow does not service server→client requests
/// (`sampling`, etc.) anyway, so all needed traffic is request/response.
pub struct HttpTransport {
    tx: Sender<Job>,
    inbound: VecDeque<String>,
    worker: Option<JoinHandle<()>>,
}

impl HttpTransport {
    /// Connect to a Streamable-HTTP MCP endpoint. `headers` are sent verbatim
    /// (auth already resolved by the caller); `bearer`, when set, becomes an
    /// `Authorization: Bearer` header.
    pub fn new(
        url: String,
        headers: Vec<(String, String)>,
        bearer: Option<String>,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let url: Arc<str> = Arc::from(url);
        let headers: Arc<Vec<(String, String)>> = Arc::new(headers);
        let bearer: Option<Arc<str>> = bearer.map(Into::into);
        let worker = thread::spawn(move || run_worker(&url, &headers, bearer.as_ref(), rx));
        Ok(Self {
            tx,
            inbound: VecDeque::new(),
            worker: Some(worker),
        })
    }
}

fn run_worker(
    url: &Arc<str>,
    headers: &Arc<Vec<(String, String)>>,
    bearer: Option<&Arc<str>>,
    rx: mpsc::Receiver<Job>,
) {
    // A private runtime keeps this transport independent of the caller's.
    let Ok(rt) = Runtime::new() else {
        for job in rx {
            let _ = job
                .reply
                .send(Err(io::Error::other("failed to start mcp http runtime")));
        }
        return;
    };
    let rt = Arc::new(rt);
    let client = build_client();
    let session: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    for job in rx {
        // One thread per request: callers may issue several MCP tool calls in
        // parallel, and each thread blocks on its own POST inside the shared
        // runtime. Detached threads end naturally after their POST finishes
        // (bounded by HTTP_TIMEOUT) even if the transport is dropped.
        let rt = Arc::clone(&rt);
        let client = client.clone();
        let session = Arc::clone(&session);
        let url = Arc::clone(url);
        let headers = Arc::clone(headers);
        let bearer = bearer.cloned();
        thread::spawn(move || {
            let lines = rt.block_on(post_once(
                &client,
                &url,
                &headers,
                bearer.as_deref(),
                &session,
                &job.body,
            ));
            let _ = job.reply.send(lines);
        });
    }
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .unwrap_or_default()
}

async fn post_once(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    bearer: Option<&str>,
    session: &Arc<Mutex<Option<String>>>,
    body: &str,
) -> io::Result<Vec<String>> {
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(id) = session.lock().ok().and_then(|guard| guard.clone()) {
        request = request.header("mcp-session-id", id);
    }

    let response = request
        .send()
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let status = response.status();
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(mut guard) = session.lock() {
            *guard = Some(id.to_string());
        }
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let text = response
        .text()
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;

    if !status.is_success() {
        let detail = if text.trim().is_empty() {
            content_type
        } else {
            text.clone()
        };
        return Err(io::Error::other(format!(
            "mcp http endpoint returned {status}: {detail}"
        )));
    }

    if content_type.contains("text/event-stream") {
        Ok(sse_jsonrpc_lines(&text))
    } else if text.trim().is_empty() {
        // Accepted notification (202) or empty response: no inbound messages.
        Ok(Vec::new())
    } else {
        Ok(vec![text])
    }
}

/// Extract JSON-RPC frames from an SSE body: each `data:` payload (multi-line
/// `data:` joined) that parses as a JSON object is returned as one line.
fn sse_jsonrpc_lines(body: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for event in body.split("\n\n") {
        let mut data = String::new();
        for field in event.lines() {
            if let Some(rest) = field.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.trim().is_empty() {
            continue;
        }
        if serde_json::from_str::<serde_json::Value>(&data).is_ok() {
            lines.push(data);
        }
    }
    lines
}

impl Transport for HttpTransport {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Job {
                body: line.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| io::Error::other("mcp http worker stopped"))?;
        let produced = reply_rx
            .recv_timeout(HTTP_TIMEOUT + Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mcp http request timed out"))??;
        self.inbound.extend(produced);
        Ok(())
    }

    fn recv_line(&mut self, _timeout: Duration) -> io::Result<Option<String>> {
        // The HTTP response was fully read during `send_line`, so pending
        // inbound frames are already buffered here.
        Ok(self.inbound.pop_front())
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        // Replace the only job-sender with a throwaway and drop the original:
        // with no senders left the worker's `for job in rx` ends, and the
        // private runtime shuts down when the worker returns.
        let _ = std::mem::replace(&mut self.tx, mpsc::channel().0);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sse_jsonrpc_lines;

    #[test]
    fn sse_frames_are_split_and_json_filtered() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n",
            "\n",
            "data: not-json\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n",
            "\n",
        );
        let lines = sse_jsonrpc_lines(body);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"id\":1"));
        assert!(lines[1].contains("\"id\":2"));
    }

    #[test]
    fn multi_line_data_frames_are_joined() {
        let body = "data: {\"a\":\ndata: 1}\n\n";
        let lines = sse_jsonrpc_lines(body);
        assert_eq!(lines, vec!["{\"a\":\n1}".to_string()]);
    }
}
