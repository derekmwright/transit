//! Agent control for transit.
//!
//! - [`Backend`]: something that answers control-protocol methods (`engine.execute`,
//!   `session.inspect`, `ui.*`…). [`Headless`] runs an in-process engine; [`Remote`] talks to a
//!   running app's `--control` port.
//! - [`mcp::Server`]: a Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0)
//!   exposing transit as tools and resources.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod mcp;
pub mod token;

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_engine::file_access::{self, AutomationRoots};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Answers control-protocol methods.
pub trait Backend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a UI is attached (screenshots, clicks).
    fn has_ui(&self) -> bool;
    fn describe(&self) -> String;
}

/// An in-process engine (no UI).
pub struct Headless {
    pub engine: Engine,
    roots: Option<Arc<AutomationRoots>>,
}

impl Headless {
    pub fn new(engine: Engine) -> Self {
        Headless { engine, roots: None }
    }

    /// Confine every file the session's commands read and write to `roots`
    /// (`soundcraft-cli mcp --automation-read-root … --automation-write-root …`).
    pub fn with_automation_roots(mut self, roots: Option<Arc<AutomationRoots>>) -> Self {
        self.roots = roots;
        self
    }

    fn answer(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "engine.execute" | "command" => {
                let id = params.get("command").or_else(|| params.get("id")).and_then(Value::as_str).ok_or("`command` required")?;
                let p = params.get("params").cloned().unwrap_or(json!({}));
                let r = self.engine.execute(id, &p).map_err(|e| e.to_string());
                // Headless has no audio engine: drop transport requests but keep the playhead.
                self.engine.transport_requests.clear();
                r
            }
            "engine.commands" => self.engine.execute("engine.commands", &params).map_err(|e| e.to_string()),
            "engine.parity" => Ok(soundcraft_engine::catalog::parity_json()),
            "session.inspect" | "document.inspect" => {
                Ok(soundcraft_engine::inspect::session(&self.engine, params.get("detail").and_then(Value::as_str) == Some("full")))
            }
            m if m.starts_with("ui.") => {
                Err(format!("`{m}` needs the desktop app: start `transit --control 0` and use `soundcraft-cli mcp --connect PORT`"))
            }
            other => Err(format!("unknown method `{other}`")),
        }
    }
}

impl Backend for Headless {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let roots = self.roots.clone();
        file_access::confine(roots.as_ref(), || self.answer(method, params))
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        format!("headless engine, session `{}`", self.engine.session().name)
    }
}

/// A TCP client for a running app's control channel.
pub struct Remote {
    addr: String,
    token: Option<String>,
    conn: Option<(TcpStream, BufReader<TcpStream>)>,
    next_id: u64,
}

enum RemoteError {
    Connection(String),
    Rejected(String),
}

impl From<String> for RemoteError {
    fn from(message: String) -> Self {
        Self::Connection(message)
    }
}

impl Remote {
    /// `target` is a port (`7979`) or `host:port`.
    pub fn new(target: &str) -> Self {
        let addr = if target.contains(':') { target.to_string() } else { format!("127.0.0.1:{target}") };
        Remote { addr, token: None, conn: None, next_id: 1 }
    }

    /// Authenticate with `token` instead of the one in the token file ([`token::token_file_path`]).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    fn connect(&mut self) -> Result<(), String> {
        if self.conn.is_some() {
            return Ok(());
        }
        let sock = self.addr.parse().map_err(|e| format!("{}: {e}", self.addr))?;
        let s = TcpStream::connect_timeout(&sock, Duration::from_millis(800)).map_err(|e| format!("cannot reach transit at {}: {e}", self.addr))?;
        s.set_read_timeout(Some(Duration::from_secs(120))).map_err(|e| e.to_string())?;
        let r = s.try_clone().map_err(|e| e.to_string())?;
        let mut s = s;
        // The app wants `auth <token>` first. Read the file on every connect: the token changes
        // each time the app starts. Without one, send nothing (older apps have no token).
        if let Some(token) = self.token.clone().or_else(token::read_token) {
            writeln!(s, "{}{token}", token::AUTH_PREFIX).map_err(|e| e.to_string())?;
        }
        self.conn = Some((s, BufReader::new(r)));
        Ok(())
    }

    fn roundtrip(&mut self, method: &str, params: &Value) -> Result<Value, RemoteError> {
        self.connect()?;
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        let (w, r) = self.conn.as_mut().ok_or_else(|| RemoteError::Connection("not connected".into()))?;
        writeln!(w, "{line}").map_err(|e| e.to_string())?;
        let mut buf = String::new();
        r.read_line(&mut buf).map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return Err(RemoteError::Connection(
                "connection closed (the app closes connections that do not start with the control token; see TRANSIT_CONTROL_TOKEN_FILE)".into(),
            ));
        }
        let v: Value = serde_json::from_str(&buf).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(RemoteError::Rejected(v.get("error").and_then(Value::as_str).unwrap_or("error").to_string()))
        }
    }
}

impl Backend for Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self.roundtrip(method, &params) {
            Ok(v) => Ok(v),
            Err(RemoteError::Connection(e)) => {
                // A write or lost response may follow an applied edit. Reconnect on the next
                // call, but never replay an operation whose outcome is unknown.
                self.conn = None;
                Err(e)
            }
            Err(RemoteError::Rejected(e)) => Err(e),
        }
    }
    fn has_ui(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        format!("transit app at {}", self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_executes_and_inspects() {
        let mut h = Headless::new(Engine::default());
        let r = h.call("engine.execute", json!({"command": "track.new", "params": {"count": 2, "format": "stereo"}}));
        assert!(r.is_ok(), "{r:?}");
        let s = h.call("session.inspect", json!({})).unwrap();
        assert_eq!(s["tracks"].as_array().map(Vec::len), Some(2));
        assert!(h.call("ui.click", json!({})).is_err());
        assert!(h.call("nope", json!({})).is_err());
    }

    #[test]
    fn remote_sends_the_auth_line_first() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let (mut first, mut second) = (String::new(), String::new());
            r.read_line(&mut first).unwrap();
            r.read_line(&mut second).unwrap();
            writeln!(&s, "{}", json!({"ok": true, "result": 1, "id": 1})).unwrap();
            (first, second)
        });
        let mut remote = Remote::new(&addr.to_string()).with_token("0123abcd");
        assert_eq!(remote.call("session.inspect", json!({})), Ok(json!(1)));
        let (first, second) = server.join().unwrap();
        assert_eq!(first, "auth 0123abcd\n");
        assert!(second.contains("session.inspect"), "{second}");
    }

    #[test]
    fn server_errors_are_not_retried_based_on_their_wording_issue_10() {
        for message in ["session closed", "Broken pipe in file export", "reset rejected"] {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (s, _) = listener.accept().unwrap();
                drop(listener); // A retry would fail to reconnect and replace the server error.
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                writeln!(&s, "{}", json!({"ok": false, "error": message})).unwrap();
            });
            let mut remote = Remote::new(&addr.to_string()).with_token("fixture");
            assert_eq!(remote.call("engine.execute", json!({})), Err(message.to_string()));
            assert!(remote.conn.is_some(), "an application error does not discard the connection");
            server.join().unwrap();
        }
    }

    #[test]
    fn a_lost_response_is_reported_without_replaying_the_command_issue_10() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(listener);
            let mut reader = BufReader::new(s);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("engine.execute"));
            // The command was received; the response is lost.
        });
        let mut remote = Remote::new(&addr.to_string()).with_token("fixture");
        let result = remote.call("engine.execute", json!({}));
        assert!(result.unwrap_err().starts_with("connection closed"));
        assert!(remote.conn.is_none());
        server.join().unwrap();
    }

    #[test]
    fn remote_reports_unreachable() {
        let mut r = Remote::new("127.0.0.1:1");
        assert!(r.call("session.inspect", json!({})).is_err());
    }
}
