//! herdr's agent-reporting socket, answered so agents that report their
//! lifecycle to herdr natively (Crush, Prime Agent) report to tutti with no
//! install step: every pane carries `HERDR_*` pointing here (`env`). The wire
//! is herdr's (v0.9.3): newline-delimited JSON on a unix socket beside the
//! session's own, one request per connection — read one line, answer one
//! line, close. This module speaks it; the server applies what a request asks.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tutti_core::{AgentState, PaneId, TabId, WorkspaceId};

/// How long a peer may take to send its request line, and the longest line
/// read: herdr's own limits.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const LINE_MAX: u64 = 1 << 20;
/// The longest `agent` — it labels a pane as an OSC 7501 `app` does, under
/// the same cap — and the longest other text a request may carry.
const AGENT_MAX: usize = 32;
const TEXT_MAX: usize = 2048;

/// What a request asks of the server.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// `notification.show`, raised on the pane it names.
    Notify {
        pane: PaneId,
        title: String,
        body: Option<String>,
    },
    /// A lifecycle report about a pane.
    Report(PaneId, Report),
}

/// One lifecycle report: who sends it (`source`) for which agent, its `seq`,
/// the conversation it names (`agent_session_id`), and what it says. `source`
/// and `agent` are always present but on a clear, which may name neither.
#[derive(Debug, PartialEq)]
pub struct Report {
    pub source: Option<String>,
    pub agent: Option<String>,
    pub seq: Option<u64>,
    pub session: Option<String>,
    pub act: Act,
}

#[derive(Debug, PartialEq)]
pub enum Act {
    /// `pane.report_agent`: the state (`None` for `unknown`) and its message.
    State(Option<AgentState>, Option<String>),
    /// `pane.report_agent_session`: the conversation alone.
    Session,
    /// `pane.release_agent`: the agent lets go of the pane.
    Release,
    /// `pane.clear_agent_authority`: its source's claim (any, naming none)
    /// ends.
    Clear,
}

impl Report {
    /// Whether this report is news under `marks`, the last seq accepted from
    /// each source: one at or below its source's mark is stale, and once a
    /// source has sent a seq, a report without one is too. Records the seq of
    /// a fresh report. A report naming no source is always fresh.
    pub fn fresh(&self, marks: &mut HashMap<String, u64>) -> bool {
        let Some(source) = &self.source else {
            return true;
        };
        match (marks.get(source), self.seq) {
            (Some(&last), Some(seq)) if seq <= last => false,
            (Some(_), None) => false,
            (_, Some(seq)) => {
                marks.insert(source.clone(), seq);
                true
            }
            (None, None) => true,
        }
    }
}

/// herdr's socket for the session whose own socket is `main`:
/// `<session>.sock` → `<session>.herdr.sock`.
pub fn socket_path(main: &Path) -> PathBuf {
    main.with_extension("herdr.sock")
}

/// Bind herdr's socket at `path`, owner-only. Whatever sits there already is
/// a dead daemon's: the session's own socket, bound first, proves no live one
/// holds it.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The env pointing a pane's herdr reporters at `socket`: herdr's
/// `w<N>:p<M>`-shaped ids, built from tutti's.
pub fn env(
    socket: &Path,
    workspace: WorkspaceId,
    tab: TabId,
    pane: PaneId,
) -> [(String, String); 5] {
    let w = format!("w{workspace}");
    [
        ("HERDR_ENV".into(), "1".into()),
        ("HERDR_SOCKET_PATH".into(), socket.display().to_string()),
        ("HERDR_PANE_ID".into(), format!("{w}:p{pane}")),
        ("HERDR_TAB_ID".into(), format!("{w}:t{tab}")),
        ("HERDR_WORKSPACE_ID".into(), w),
    ]
}

/// Answer requests on `listener` until the task is aborted, each connection
/// on its own task so a slow peer stalls only itself. `handle` applies a
/// request and says whether its pane exists.
pub async fn serve(
    listener: UnixListener,
    handle: impl Fn(Request) -> bool + Send + Sync + 'static,
) {
    let handle = Arc::new(handle);
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let handle = Arc::clone(&handle);
                tokio::spawn(async move { answer(stream, &*handle).await });
            }
            // Out of descriptors, say: back off instead of spinning on it.
            Err(err) => {
                eprintln!("tutti: herdr socket accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Read one request line, answer it, close. A peer that sends no line within
/// `READ_TIMEOUT` is dropped unanswered.
async fn answer(stream: UnixStream, handle: &impl Fn(Request) -> bool) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(LINE_MAX));
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line);
    if !matches!(tokio::time::timeout(READ_TIMEOUT, read).await, Ok(Ok(_))) {
        return;
    }
    let (id, asked) = if line.ends_with(b"\n") {
        parse(&line)
    } else {
        let error = Error::invalid("a request is one newline-terminated line, under 1 MiB");
        (Value::Null, Err(error))
    };
    let reply = asked.and_then(|ask| match ask {
        // The herdr release whose protocol this speaks.
        Ask::Ping => Ok(json!({
            "type": "pong", "version": "0.9.3+tutti", "protocol": 22, "server": "tutti",
        })),
        Ask::Metadata => Ok(json!({"type": "ok"})),
        Ask::Pane(pane, request) => {
            if handle(request) {
                Ok(json!({"type": "ok"}))
            } else {
                Err(not_found(&pane))
            }
        }
    });
    let line = match reply {
        Ok(result) => json!({"id": id, "result": result}),
        Err(Error { code, message }) => {
            json!({"id": id, "error": {"code": code, "message": message}})
        }
    };
    let _ = write.write_all(format!("{line}\n").as_bytes()).await;
}

/// An error reply: herdr's `code` and a message for humans.
#[derive(Debug, PartialEq)]
struct Error {
    code: &'static str,
    message: String,
}

impl Error {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
        }
    }
}

fn not_found(pane: &str) -> Error {
    Error {
        code: "pane_not_found",
        message: format!("pane {pane} not found"),
    }
}

/// What one request line asks.
#[derive(Debug, PartialEq)]
enum Ask {
    Ping,
    /// `pane.report_metadata`: display-only, so acknowledged and dropped.
    Metadata,
    /// A request about the pane named by its `pane_id`, kept as sent.
    Pane(String, Request),
}

/// A request's `params`, every method's fields in one. herdr reads an empty
/// string as absent (Crush always sends `agent_session_id`) and ignores
/// fields it does not know (`resume_argv`, `agent_session_path`, …).
#[derive(Default, Deserialize)]
#[serde(default)]
struct Params {
    #[serde(deserialize_with = "text")]
    pane_id: Option<String>,
    #[serde(deserialize_with = "text")]
    source: Option<String>,
    #[serde(deserialize_with = "text")]
    agent: Option<String>,
    #[serde(deserialize_with = "text")]
    state: Option<String>,
    #[serde(deserialize_with = "text")]
    message: Option<String>,
    seq: Option<u64>,
    #[serde(deserialize_with = "text")]
    agent_session_id: Option<String>,
    #[serde(deserialize_with = "text")]
    title: Option<String>,
    #[serde(deserialize_with = "text")]
    body: Option<String>,
}

/// A text field: absent when empty, refused when longer than `TEXT_MAX` or
/// holding a control character — it may reach a terminal, as a notification
/// is re-raised on the user's own.
fn text<'de, D: Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    let text = Option::<String>::deserialize(de)?.filter(|t| !t.is_empty());
    match &text {
        Some(t) if t.len() > TEXT_MAX || t.chars().any(char::is_control) => Err(
            serde::de::Error::custom("a text field is over-long or holds a control character"),
        ),
        _ => Ok(text),
    }
}

/// One request line's `id`, echoed in the reply (`null` when it has none),
/// and what it asks.
fn parse(line: &[u8]) -> (Value, Result<Ask, Error>) {
    let value: Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(err) => return (Value::Null, Err(Error::invalid(err.to_string()))),
    };
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    (id, ask(value))
}

fn ask(value: Value) -> Result<Ask, Error> {
    #[derive(Deserialize)]
    struct Envelope {
        method: String,
        #[serde(default)]
        params: Option<Params>,
    }
    let Envelope { method, params } =
        Envelope::deserialize(value).map_err(|err| Error::invalid(err.to_string()))?;
    let p = params.unwrap_or_default();
    let pane = match p.pane_id {
        None => Err(Error::invalid("pane_id is required")),
        Some(id) => match pane_of(&id) {
            Some(pane) => Ok((id, pane)),
            None => Err(not_found(&id)),
        },
    };
    let act = match method.as_str() {
        "ping" => return Ok(Ask::Ping),
        "pane.report_metadata" => return Ok(Ask::Metadata),
        "notification.show" => {
            let (id, pane) = pane?;
            let title = p.title.ok_or_else(|| Error::invalid("title is required"))?;
            let body = p.body;
            return Ok(Ask::Pane(id, Request::Notify { pane, title, body }));
        }
        "pane.report_agent" => {
            let state = match p.state.as_deref() {
                Some("idle") => Some(AgentState::Idle),
                Some("working") => Some(AgentState::Working),
                Some("blocked") => Some(AgentState::Blocked),
                Some("unknown") => None,
                other => return Err(Error::invalid(format!("unknown state {other:?}"))),
            };
            Act::State(state, p.message)
        }
        "pane.report_agent_session" => Act::Session,
        "pane.release_agent" => Act::Release,
        "pane.clear_agent_authority" => Act::Clear,
        other => return Err(Error::invalid(format!("unknown method {other:?}"))),
    };
    let (id, pane) = pane?;
    if act != Act::Clear {
        if p.source.is_none() {
            return Err(Error::invalid("source is required"));
        }
        if !p.agent.as_ref().is_some_and(|a| a.len() <= AGENT_MAX) {
            return Err(Error {
                code: "invalid_agent",
                message: format!("agent must be 1 to {AGENT_MAX} bytes"),
            });
        }
    }
    let report = Report {
        source: p.source,
        agent: p.agent,
        seq: p.seq,
        session: p.agent_session_id,
        act,
    };
    Ok(Ask::Pane(id, Request::Report(pane, report)))
}

/// The tutti pane a `w<N>:p<M>` id (`env`'s) names.
fn pane_of(id: &str) -> Option<PaneId> {
    let (workspace, pane) = id.strip_prefix('w')?.split_once(":p")?;
    workspace.parse::<u64>().ok()?;
    pane.parse().ok().map(PaneId)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `request` asks, or the code it is refused with.
    fn asked(request: Value) -> Result<Ask, &'static str> {
        parse(format!("{request}\n").as_bytes())
            .1
            .map_err(|error| error.code)
    }

    /// The act `request` reports, or the code it is refused with.
    fn act(request: Value) -> Result<Act, &'static str> {
        asked(request).map(|ask| match ask {
            Ask::Pane(_, Request::Report(_, report)) => report.act,
            other => panic!("not a report: {other:?}"),
        })
    }

    /// A `method` request about pane `w2:p5` from Prime Agent, with `extra`
    /// params set over the defaults.
    fn request(method: &str, extra: &[(&str, Value)]) -> Value {
        let mut params = json!({"pane_id": "w2:p5", "source": "herdr:pi", "agent": "prime-agent"});
        for (key, value) in extra {
            params[*key] = value.clone();
        }
        json!({"id": "r1", "method": method, "params": params})
    }

    #[test]
    fn the_env_names_its_pane_back() {
        let socket = socket_path(Path::new("/tmp/tutti-501/work.sock"));
        assert_eq!(socket, Path::new("/tmp/tutti-501/work.herdr.sock"));
        assert_eq!(
            env(&socket, WorkspaceId(2), TabId(3), PaneId(5)).map(|(k, v)| format!("{k}={v}")),
            [
                "HERDR_ENV=1",
                "HERDR_SOCKET_PATH=/tmp/tutti-501/work.herdr.sock",
                "HERDR_PANE_ID=w2:p5",
                "HERDR_TAB_ID=w2:t3",
                "HERDR_WORKSPACE_ID=w2",
            ]
        );
        assert_eq!(pane_of("w2:p5"), Some(PaneId(5)));
        for junk in ["p5", "2:p5", "w:p5", "w2:t5", "w2:p", "w2:p5x"] {
            assert_eq!(pane_of(junk), None, "{junk}");
        }
    }

    /// Crush's own shapes: its UnixNano seq survives past 2^53, and the
    /// `agent_session_id` it always sends reads absent when empty.
    #[test]
    fn crush_shaped_requests_parse() {
        let crush = |seq: u64, session: Option<&str>, act| {
            let report = Report {
                source: Some("crush".into()),
                agent: Some("crush".into()),
                seq: Some(seq),
                session: session.map(Into::into),
                act,
            };
            Ok(Ask::Pane(
                "w1:p1".into(),
                Request::Report(PaneId(1), report),
            ))
        };
        assert_eq!(
            asked(json!({
                "id": "crush:report:1791590400123456789",
                "method": "pane.report_agent",
                "params": {"pane_id": "w1:p1", "source": "crush", "agent": "crush",
                    "state": "working", "seq": 1_791_590_400_123_456_790_u64, "agent_session_id": ""},
            })),
            crush(
                1_791_590_400_123_456_790,
                None,
                Act::State(Some(AgentState::Working), None)
            )
        );
        assert_eq!(
            asked(json!({
                "id": "crush:release:1791590400999999999",
                "method": "pane.release_agent",
                "params": {"pane_id": "w1:p1", "source": "crush", "agent": "crush",
                    "state": "", "seq": 1_791_590_400_999_999_998_u64, "agent_session_id": "s1"},
            })),
            crush(1_791_590_400_999_999_998, Some("s1"), Act::Release)
        );
    }

    #[test]
    fn each_method_parses() {
        use AgentState::{Blocked, Idle};
        let state = |state: &str| request("pane.report_agent", &[("state", json!(state))]);
        assert_eq!(act(state("idle")), Ok(Act::State(Some(Idle), None)));
        assert_eq!(act(state("unknown")), Ok(Act::State(None, None)));
        assert_eq!(
            act(request(
                "pane.report_agent",
                &[
                    ("state", json!("blocked")),
                    ("message", json!("allow bash?"))
                ]
            )),
            Ok(Act::State(Some(Blocked), Some("allow bash?".into())))
        );
        assert_eq!(
            act(request("pane.report_agent_session", &[])),
            Ok(Act::Session)
        );
        assert_eq!(act(request("pane.release_agent", &[])), Ok(Act::Release));
        let anyone = [("source", json!("")), ("agent", json!(""))];
        assert_eq!(
            act(request("pane.clear_agent_authority", &anyone)),
            Ok(Act::Clear),
            "a clear may name neither"
        );
        assert_eq!(
            asked(request(
                "notification.show",
                &[("title", json!("Done")), ("body", json!(""))]
            )),
            Ok(Ask::Pane(
                "w2:p5".into(),
                Request::Notify {
                    pane: PaneId(5),
                    title: "Done".into(),
                    body: None,
                }
            ))
        );
        assert_eq!(asked(json!({"method": "ping"})), Ok(Ask::Ping));
        let go_nil = json!({"method": "ping", "params": null});
        assert_eq!(asked(go_nil), Ok(Ask::Ping), "a Go client's nil params");
        let metadata = json!({"method": "pane.report_metadata", "params": {"title": "x"}});
        assert_eq!(asked(metadata), Ok(Ask::Metadata));
    }

    #[test]
    fn bad_requests_get_herdrs_error_codes() {
        let report = |extra: &[(&str, Value)]| {
            let mut params = vec![("state", json!("idle"))];
            params.extend_from_slice(extra);
            request("pane.report_agent", &params)
        };
        for (request, code) in [
            (report(&[("state", json!(null))]), "invalid_request"),
            (report(&[("state", json!("done"))]), "invalid_request"),
            (report(&[("agent", json!(""))]), "invalid_agent"),
            (report(&[("agent", json!("a".repeat(33)))]), "invalid_agent"),
            (report(&[("source", json!(""))]), "invalid_request"),
            (report(&[("pane_id", json!(""))]), "invalid_request"),
            (report(&[("pane_id", json!("p5"))]), "pane_not_found"),
            (
                report(&[("message", json!("a\u{1b}[2Jb"))]),
                "invalid_request",
            ),
            (
                report(&[("message", json!("m".repeat(TEXT_MAX + 1)))]),
                "invalid_request",
            ),
            (report(&[("seq", json!(-1))]), "invalid_request"),
            (report(&[("seq", json!("7"))]), "invalid_request"),
            (request("notification.show", &[]), "invalid_request"),
            (request("pane.dance", &[]), "invalid_request"),
        ] {
            assert_eq!(asked(request.clone()).err(), Some(code), "{request}");
        }
        let (id, refused) = parse(b"{not json\n");
        assert_eq!(
            (id, refused.unwrap_err().code),
            (Value::Null, "invalid_request")
        );
        let (id, refused) = parse(b"{\"id\":\"r9\",\"params\":{}}\n");
        assert_eq!(
            (id, refused.unwrap_err().code),
            (json!("r9"), "invalid_request"),
            "a request with no method still gets its id back"
        );
    }

    #[test]
    fn seq_marks_count_per_source() {
        let mut marks = HashMap::new();
        let report = |source: Option<&str>, seq| Report {
            source: source.map(Into::into),
            agent: None,
            seq,
            session: None,
            act: Act::Release,
        };
        let crush = |seq| report(Some("crush"), seq);
        assert!(crush(None).fresh(&mut marks), "no seq seen yet");
        assert!(crush(Some(5)).fresh(&mut marks));
        assert!(!crush(Some(5)).fresh(&mut marks), "a repeat is stale");
        assert!(!crush(Some(4)).fresh(&mut marks), "so is an older one");
        assert!(
            !crush(None).fresh(&mut marks),
            "and, once one was seen, none"
        );
        assert!(crush(Some(6)).fresh(&mut marks));
        assert!(
            report(Some("herdr:pi"), Some(1)).fresh(&mut marks),
            "per source"
        );
        assert!(
            report(None, Some(1)).fresh(&mut marks),
            "a clear naming none"
        );
        assert_eq!(
            marks,
            HashMap::from([("crush".to_string(), 6), ("herdr:pi".to_string(), 1)])
        );
    }

    /// Over a real socket: it is owner-only and replaces a stale file, and each
    /// connection gets one reply line, then EOF.
    #[tokio::test]
    async fn the_socket_answers_one_line_per_connection() {
        let dir = std::env::temp_dir().join(format!("tutti-herdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.herdr.sock");
        std::fs::write(&path, "stale").unwrap();
        let listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let (seen, mut handled) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve(listener, move |request| {
            let found = !matches!(request, Request::Report(PaneId(99), _));
            seen.send(request).unwrap();
            found
        }));
        let exchange = async |line: &str, close: bool| {
            let mut stream = UnixStream::connect(&path).await.unwrap();
            stream.write_all(line.as_bytes()).await.unwrap();
            if close {
                stream.shutdown().await.unwrap();
            }
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            assert_eq!(reply.matches('\n').count(), 1, "one line: {reply:?}");
            serde_json::from_str::<Value>(&reply).unwrap()
        };

        let pong = exchange("{\"id\":1,\"method\":\"ping\"}\n", false).await;
        assert_eq!(pong["id"], 1);
        assert_eq!(pong["result"]["type"], "pong");
        let release = request("pane.release_agent", &[]);
        assert_eq!(
            exchange(&format!("{release}\n"), false).await,
            json!({"id": "r1", "result": {"type": "ok"}})
        );
        assert!(matches!(
            handled.recv().await,
            Some(Request::Report(
                PaneId(5),
                Report {
                    act: Act::Release,
                    ..
                }
            ))
        ));
        let gone = request("pane.release_agent", &[("pane_id", json!("w1:p99"))]);
        assert_eq!(
            exchange(&format!("{gone}\n"), false).await["error"]["code"],
            "pane_not_found"
        );
        assert_eq!(
            exchange(&format!("{release}"), true).await["error"]["code"],
            "invalid_request",
            "a request must end its line"
        );
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
