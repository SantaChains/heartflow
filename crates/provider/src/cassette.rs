//! Provider cassette: record a real turn's provider stream to disk, then replay
//! it offline with no network and no provider key.
//!
//! **Why this exists.** Without a live endpoint, the agent loop — tool
//! round-trips, permission gates, mid-turn compaction, cancellation — can only
//! be exercised by the scripted `ApiClient` doubles inside `runtime`'s test
//! module. Those prove the loop is correct *for a given script*; they cannot
//! capture what a real endpoint actually sent. A cassette closes that gap:
//! `HEARTFLOW_CASSETTE=record:FILE` tees a live session to disk and
//! `replay:FILE` re-drives the real `ConversationRuntime` from that file. Once a
//! session is recorded, the loop regression it encodes stays runnable forever,
//! on any machine, with no credentials.
//!
//! **Fidelity boundary.** Recording happens at the [`AgentEvent`] level
//! (post-parse), not at the wire level. That is deliberate: the target is the
//! *loop*, and SSE parsing already has socket-level coverage in
//! `crates/api/tests/client_integration.rs`. A cassette therefore does **not**
//! re-test provider parsing.
//!
//! **Replay contract.** Entries are consumed in order, and each incoming
//! [`ApiRequest`] must equal the recorded one. Divergence fails loudly and names
//! the first differing section, which makes a cassette double as a
//! prompt/tool-assembly regression fixture: if prompt construction drifts, the
//! replay says so instead of silently accepting a request nothing recorded.
//!
//! **Known limitation.** A multi-turn replay only reproduces if the *tool* side
//! is deterministic too, because each request carries the previous turn's tool
//! results. Recording tool output as well (a full effect log) is the natural
//! next step; this module stops at the provider boundary on purpose.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{AgentEvent, ApiClient, ApiRequest, RuntimeError, TurnStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

/// Environment variable selecting the mode: `record:<path>` or `replay:<path>`.
pub const CASSETTE_ENV: &str = "HEARTFLOW_CASSETTE";

/// Bumped when the on-disk shape changes in a way older readers cannot load.
const FORMAT_VERSION: u32 = 1;

/// Outbound queue depth of the recording relay. Bounded on purpose: a consumer
/// slower than the relay must apply backpressure to the provider task, exactly
/// as a direct stream would, rather than letting the queue grow unbounded.
const RELAY_CAPACITY: usize = 64;

/// What a [`CassetteClient`] does with each turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CassetteMode {
    /// Pass through untouched. The default, and the only mode that never
    /// interposes a task.
    Off,
    /// Forward to the provider and write the resulting events to `path`.
    Record(PathBuf),
    /// Serve events from `path` without touching the network.
    Replay(PathBuf),
}

impl CassetteMode {
    /// Parse a `HEARTFLOW_CASSETTE` value.
    ///
    /// Splits on the **first** colon only, so a Windows drive letter in the path
    /// (`record:C:\tmp\a.json`) survives intact.
    ///
    /// # Errors
    /// Returns the offending value in the message so a caller can report it
    /// verbatim rather than guessing what was meant.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let Some((kind, path)) = raw.split_once(':') else {
            return Err(format!(
                "{CASSETTE_ENV} must be `record:<path>` or `replay:<path>`, got `{raw}`"
            ));
        };
        if path.trim().is_empty() {
            return Err(format!("{CASSETTE_ENV} has an empty path in `{raw}`"));
        }
        match kind.trim() {
            "record" => Ok(Self::Record(PathBuf::from(path))),
            "replay" => Ok(Self::Replay(PathBuf::from(path))),
            other => Err(format!(
                "{CASSETTE_ENV} mode must be `record` or `replay`, got `{other}`"
            )),
        }
    }

    /// Read [`CASSETTE_ENV`]. `Ok(None)` when unset or blank.
    ///
    /// # Errors
    /// A malformed value is an error rather than a silent `Off`: a typo'd path
    /// that quietly disables recording is worse than a hard stop at startup.
    pub fn from_env() -> Result<Option<Self>, String> {
        match std::env::var(CASSETTE_ENV) {
            Ok(raw) if !raw.trim().is_empty() => Self::parse(&raw).map(Some),
            _ => Ok(None),
        }
    }

    /// The path this mode reads from or writes to, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Off => None,
            Self::Record(path) | Self::Replay(path) => Some(path),
        }
    }
}

/// One recorded turn: the request that produced it, and what came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    request: ApiRequest,
    events: Vec<AgentEvent>,
}

/// On-disk cassette. `version` is checked on load so a future format change
/// reports itself instead of failing as a confusing serde error.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cassette {
    version: u32,
    recorded_at_unix_secs: u64,
    entries: Vec<Entry>,
}

/// Wraps a transport so a live session can be recorded and replayed.
///
/// `C` may be `None`-backed in replay mode — see [`CassetteClient::replay`] —
/// which is what lets a recorded session run with no provider key configured.
pub struct CassetteClient<C> {
    inner: Option<C>,
    mode: CassetteMode,
    /// Events recorded so far, shared with the in-flight relay task.
    recorded: Arc<Mutex<Vec<Entry>>>,
    /// Replay cursor, filled on first use.
    pending: VecDeque<Entry>,
    replay_loaded: bool,
    /// Turns already served in replay mode, for error messages.
    served: usize,
}

impl<C: ApiClient> CassetteClient<C> {
    /// Wrap a live transport. `mode` is normally [`CassetteMode::Off`] or
    /// [`CassetteMode::Record`].
    #[must_use]
    pub fn live(inner: C, mode: CassetteMode) -> Self {
        Self {
            inner: Some(inner),
            mode,
            recorded: Arc::new(Mutex::new(Vec::new())),
            pending: VecDeque::new(),
            replay_loaded: false,
            served: 0,
        }
    }

    /// Build a replay-only client. No transport and therefore no credentials are
    /// required, which is the whole point: a recorded session must be runnable
    /// offline.
    #[must_use]
    pub fn replay(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: None,
            mode: CassetteMode::Replay(path.into()),
            recorded: Arc::new(Mutex::new(Vec::new())),
            pending: VecDeque::new(),
            replay_loaded: false,
            served: 0,
        }
    }

    /// Wrap `inner`, honouring [`CASSETTE_ENV`].
    ///
    /// A malformed value degrades to [`CassetteMode::Off`] with a warning rather
    /// than failing the session: the cassette is a diagnostic, and it must never
    /// be the reason a real turn cannot run. Callers that want the opposite
    /// trade-off should use [`CassetteMode::from_env`] and fail fast themselves.
    #[must_use]
    pub fn from_env(inner: C) -> Self {
        match CassetteMode::from_env() {
            Ok(Some(mode)) => Self::live(inner, mode),
            Ok(None) => Self::live(inner, CassetteMode::Off),
            Err(message) => {
                warn!("{message}; cassette disabled");
                Self::live(inner, CassetteMode::Off)
            }
        }
    }

    /// Events recorded by this client (empty unless recording).
    #[must_use]
    pub fn recorded_len(&self) -> usize {
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn live_inner(&mut self) -> Result<&mut C, RuntimeError> {
        self.inner.as_mut().ok_or_else(|| {
            RuntimeError::new("cassette: no transport configured for a non-replay mode")
        })
    }

    fn load_replay(&mut self, path: &Path) -> Result<(), RuntimeError> {
        if self.replay_loaded {
            return Ok(());
        }
        let text = fs::read_to_string(path)
            .map_err(|error| RuntimeError::new(format!("cassette {}: {error}", path.display())))?;
        let cassette: Cassette = serde_json::from_str(&text).map_err(|error| {
            RuntimeError::new(format!(
                "cassette {} is not a valid cassette: {error}",
                path.display()
            ))
        })?;
        if cassette.version != FORMAT_VERSION {
            return Err(RuntimeError::new(format!(
                "cassette {} has format version {} but this build understands {FORMAT_VERSION}",
                path.display(),
                cassette.version
            )));
        }
        self.pending = cassette.entries.into();
        self.replay_loaded = true;
        Ok(())
    }
}

impl<C: ApiClient> ApiClient for CassetteClient<C> {
    fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError> {
        match self.mode.clone() {
            // The default path adds nothing: no relay task, no buffering, no
            // behavioural delta relative to talking to the transport directly.
            CassetteMode::Off => self.live_inner()?.stream(request),

            CassetteMode::Record(path) => {
                let inner = self.live_inner()?;
                let mut upstream = inner.stream(request.clone())?;
                let (tx, rx) = mpsc::channel(RELAY_CAPACITY);
                let recorder = TurnRecorder::new(path, Arc::clone(&self.recorded), request);
                let handle = tokio::spawn(async move {
                    let mut recorder = recorder;
                    while let Some(event) = upstream.recv().await {
                        recorder.events.push(event.clone());
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    // Explicit so the write completes before `tx` drops, and
                    // therefore before the consumer observes end-of-stream; a
                    // reader that starts right after the turn cannot race it.
                    // `Drop` covers the aborted case, where this is unreachable.
                    recorder.publish();
                });
                Ok(TurnStream::new(rx, handle.abort_handle()))
            }

            CassetteMode::Replay(path) => {
                self.load_replay(&path)?;
                let turn = self.served + 1;
                let Some(entry) = self.pending.pop_front() else {
                    return Err(RuntimeError::new(format!(
                        "cassette {} has no entry for turn {turn}: the recording ended earlier \
                         than this run (it has {} entries)",
                        path.display(),
                        turn - 1
                    )));
                };
                if entry.request != request {
                    return Err(RuntimeError::new(format!(
                        "cassette {} turn {turn} does not match this run: {}",
                        path.display(),
                        explain_mismatch(&entry.request, &request)
                    )));
                }
                self.served = turn;
                Ok(TurnStream::from_events(entry.events))
            }
        }
    }
}

/// Persists one turn when it ends.
///
/// Two callers, one idempotent body: the relay calls [`Self::publish`] when the
/// upstream stream ends, and `Drop` calls it again to cover the aborted case —
/// aborting a task drops its future, which still runs destructors, so a turn the
/// user cancelled is recorded with the events that did arrive.
struct TurnRecorder {
    path: PathBuf,
    recorded: Arc<Mutex<Vec<Entry>>>,
    request: Option<ApiRequest>,
    events: Vec<AgentEvent>,
}

impl TurnRecorder {
    fn new(path: PathBuf, recorded: Arc<Mutex<Vec<Entry>>>, request: ApiRequest) -> Self {
        Self {
            path,
            recorded,
            request: Some(request),
            events: Vec::new(),
        }
    }

    /// Append this turn to the cassette on disk.
    ///
    /// Taking the request out makes a second call a no-op, which is what lets
    /// the explicit call and the destructor coexist.
    fn publish(&mut self) {
        let Some(request) = self.request.take() else {
            return;
        };
        let entry = Entry {
            request,
            events: std::mem::take(&mut self.events),
        };
        let mut guard = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(entry);
        if let Err(error) = persist(&self.path, &guard) {
            warn!("cassette {}: {error}", self.path.display());
        }
    }
}

impl Drop for TurnRecorder {
    fn drop(&mut self) {
        // Reached with a live request only on the abort path — the turn was
        // cancelled before the relay's own `publish`. Blocking file I/O in a
        // destructor is acceptable here: one small file per turn, and `Drop`
        // cannot be async.
        self.publish();
    }
}

fn persist(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let cassette = Cassette {
        version: FORMAT_VERSION,
        recorded_at_unix_secs: unix_secs(),
        entries: entries.to_vec(),
    };
    let text = serde_json::to_string_pretty(&cassette)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    fs::write(path, text)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Name the first structural difference. Deliberately coarse — section and
/// index, not a diff — because the useful signal is *that* prompt assembly
/// drifted, and a full dump of two message trees would bury it.
fn explain_mismatch(recorded: &ApiRequest, live: &ApiRequest) -> String {
    if recorded.system_prompt != live.system_prompt {
        return format!(
            "system prompt differs (recorded {} blocks, live {})",
            recorded.system_prompt.len(),
            live.system_prompt.len()
        );
    }
    if recorded.tools != live.tools {
        return format!(
            "tool set differs (recorded {} tools, live {})",
            recorded.tools.len(),
            live.tools.len()
        );
    }
    if recorded.messages != live.messages {
        let first = recorded
            .messages
            .iter()
            .zip(&live.messages)
            .position(|(a, b)| a != b);
        return match first {
            Some(index) => format!(
                "message {index} differs (role {:?})",
                live.messages[index].role
            ),
            None => format!(
                "message count differs (recorded {}, live {})",
                recorded.messages.len(),
                live.messages.len()
            ),
        };
    }
    "the requests compare unequal but no section differs".to_string()
}
