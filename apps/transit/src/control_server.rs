// Copied from apps/soundcraft/src/control_server.rs at transit 2af868e (file last changed 74b3278); only the
// request type and the printed port variable differ.
//! Loopback JSON-lines control server: one request per line, one reply per line.
//! This is the transport the MCP server (`soundcraft-cli mcp --connect PORT`) wraps.
//!
//! Every connection must send `auth <token>` as its first line, the per-run token that `start`
//! writes to the `control-token` file (see `soundcraft_automation::token`); otherwise it is closed
//! without a reply, as it is after a wrong token or a parse error. The bind is loopback only.
//!
//! Lines are bounded ([`MAX_LINE`]): a longer one is refused and the connection closed, without
//! buffering the rest. So is the number of connections served at once ([`MAX_CONNECTIONS`]).

use serde_json::{Value, json};
use soundcraft_automation::token;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use transit_ui_studio::ControlRequest;

/// Longest accepted request line (bytes, without the newline). The largest payload a request
/// carries is a plugin state for `mix.insert_state`: base64 of at most `b64::MAX_DECODED_BYTES`
/// (64 MiB, so about 85 MiB of text). Room for that and 1 MiB of JSON around it; replies
/// (screenshots) aren't bounded by this.
const MAX_LINE: usize = soundcraft_engine::model::b64::MAX_DECODED_BYTES.div_ceil(3) * 4 + (1 << 20);
// With MAX_CONNECTIONS, at most 1.5 GiB of request lines are buffered at once.
const _: () = assert!(MAX_LINE <= 96 << 20);
/// Connections served at once; further ones get an error line and are closed.
const MAX_CONNECTIONS: usize = 16;
/// Longest accepted `auth` line; the token is 32 characters.
const MAX_AUTH_LINE: usize = 256;

/// How long a new connection has to send its `auth` line.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

pub fn start(port: u16, ctx: egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = channel::<ControlRequest>();
    // Without a token nobody could authenticate, so the server does not start.
    let Some(token_path) = token::token_file_path() else {
        log::error!("control server not started: no settings folder for the token file (set TRANSIT_CONTROL_TOKEN_FILE)");
        return rx;
    };
    let secret = match token::generate().and_then(|t| token::write_token_file(&token_path, &t).map(|()| t)) {
        Ok(t) => Arc::<str>::from(t),
        Err(e) => {
            log::error!("control server not started: {e}");
            return rx;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("control server failed to bind 127.0.0.1:{port}: {e}");
            return rx;
        }
    };
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    log::info!("control server listening on 127.0.0.1:{actual}; token file {}", token_path.display());
    println!("TRANSIT_CONTROL_PORT={actual}");
    // The same line as SoundCraft prints, so scripts written for `soundcraft.exe` find the port too.
    println!("SOUNDCRAFT_CONTROL_PORT={actual}");
    let spawned = std::thread::Builder::new().name("transit-control".into()).spawn(move || accept_loop(listener, tx, ctx, secret));
    if let Err(e) = spawned {
        log::error!("control thread failed: {e}");
    }
    rx
}

/// A connection's place among [`MAX_CONNECTIONS`], given back when the connection ends (also when
/// its thread can't be started, or panics).
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, tx: Sender<ControlRequest>, ctx: egui::Context, secret: Arc<str>) {
    let open = Arc::new(AtomicUsize::new(0));
    for mut stream in listener.incoming().flatten() {
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "too many control connections"}));
            continue;
        }
        let slot = Slot(Arc::clone(&open));
        let tx = tx.clone();
        let ctx = ctx.clone();
        let secret = Arc::clone(&secret);
        let _ = std::thread::Builder::new().name("transit-control-conn".into()).spawn(move || {
            let _slot = slot;
            serve(stream, tx, ctx, &secret);
        });
    }
}

/// Read one `\n`-terminated line of at most `max` bytes into `buf` (newline stripped).
/// `Ok(false)` at end of stream; `Err` when the line is too long or the read fails.
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> Result<bool, String> {
    buf.clear();
    // `take` bounds the bytes buffered for one line: a peer can't make us allocate more than `max`.
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let n = reader.take(limit).read_until(b'\n', buf).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(true);
    }
    if buf.len() > max {
        return Err(format!("request line longer than {max} bytes; closing the connection"));
    }
    // Last line without a newline, then end of stream.
    Ok(true)
}

fn serve(stream: TcpStream, tx: Sender<ControlRequest>, ctx: egui::Context, secret: &str) {
    let Ok(read) = stream.try_clone() else { return };
    // A peer that never sends its auth line must not hold a slot.
    let _ = read.set_read_timeout(Some(AUTH_TIMEOUT));
    let timeout_off = read.try_clone();
    let mut out = stream;
    let mut reader = BufReader::new(read);
    handle(
        &mut reader,
        &mut out,
        secret,
        |method, params| {
            let (req, rrx) = ControlRequest::new(method, params);
            tx.send(req).ok()?;
            ctx.request_repaint();
            Some(rrx.recv_timeout(Duration::from_secs(90)).unwrap_or_else(|_| json!({"ok": false, "error": "timeout"})))
        },
        || {
            if let Ok(s) = &timeout_off {
                let _ = s.set_read_timeout(None);
            }
        },
    );
}

/// One connection: the `auth` line, then requests until the peer leaves, sends something invalid or
/// `call` returns `None`. `call` answers one request (`None` = the app is gone); `authenticated`
/// runs once the token has been accepted.
fn handle(
    reader: &mut impl BufRead,
    out: &mut impl Write,
    secret: &str,
    mut call: impl FnMut(String, Value) -> Option<Value>,
    authenticated: impl FnOnce(),
) {
    let mut buf = Vec::new();
    // The first line must be `auth <token>`. Anything else closes the connection without a reply.
    match read_bounded_line(reader, &mut buf, MAX_AUTH_LINE) {
        Ok(true) => {}
        _ => return,
    }
    let Some(given) = std::str::from_utf8(&buf).ok().and_then(|l| l.strip_prefix(token::AUTH_PREFIX)) else { return };
    if !token::matches(secret, given.trim_end()) {
        return;
    }
    authenticated();
    loop {
        match read_bounded_line(reader, &mut buf, MAX_LINE) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                let _ = writeln!(out, "{}", json!({"ok": false, "error": e}));
                break;
            }
        }
        // Bytes that aren't UTF-8 end the connection, as `BufRead::lines` did.
        let Ok(line) = std::str::from_utf8(&buf) else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let Some(mut r) = call(method, params) else { break };
                if let Some(o) = r.as_object_mut() {
                    o.insert("id".into(), id);
                }
                r
            }
            Err(e) => {
                // A line that isn't JSON (an HTTP request, say) is not a client of ours: answer once, then close.
                let _ = writeln!(out, "{}", json!({"ok": false, "error": format!("bad JSON: {e}; closing the connection")}));
                break;
            }
        };
        if writeln!(out, "{reply}").is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_read_up_to_the_limit() {
        let mut buf = Vec::new();
        let mut input: &[u8] = b"aaaaaaaaaa\nbb\r\ncc";
        assert_eq!(read_bounded_line(&mut input, &mut buf, 10), Ok(true));
        assert_eq!(buf, b"aaaaaaaaaa", "a line exactly at the limit is accepted");
        assert_eq!(read_bounded_line(&mut input, &mut buf, 10), Ok(true));
        assert_eq!(buf, b"bb", "CRLF is stripped");
        assert_eq!(read_bounded_line(&mut input, &mut buf, 10), Ok(true));
        assert_eq!(buf, b"cc", "a last line without a newline is still read");
        assert_eq!(read_bounded_line(&mut input, &mut buf, 10), Ok(false), "then end of stream");
    }

    #[test]
    fn an_overlong_line_is_refused_without_reading_the_rest() {
        let mut buf = Vec::new();
        let long = [b'x'; 100];
        let mut input: &[u8] = &long;
        assert!(read_bounded_line(&mut input, &mut buf, 10).is_err());
        assert!(buf.len() <= 11, "at most limit + 1 bytes are buffered, got {}", buf.len());
        assert_eq!(input.len(), 100 - 11, "the rest of the line is left unread");
    }

    #[test]
    fn the_largest_plugin_state_fits_in_a_line() {
        let state = soundcraft_engine::model::b64::MAX_DECODED_BYTES.div_ceil(3) * 4;
        let request = json!({"id": 1, "method": "engine.execute", "params": {"command": "mix.insert_state", "params": {"track": "Audio 1", "slot": 0, "state": ""}}});
        assert!(state + request.to_string().len() <= MAX_LINE);
    }

    /// The control server on a loopback port, with a stand-in UI that echoes each method.
    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn serve_echo() -> std::net::SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = channel::<ControlRequest>();
        std::thread::spawn(move || accept_loop(listener, tx, egui::Context::default(), Arc::from(TEST_TOKEN)));
        std::thread::spawn(move || {
            for req in rx {
                let _ = req.reply.send(json!({"ok": true, "method": req.method}));
            }
        });
        addr
    }

    #[test]
    fn requests_are_answered_with_their_ids() {
        let addr = serve_echo();
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(format!("auth {TEST_TOKEN}\n").as_bytes()).unwrap();
        c.write_all(b"{\"id\":3,\"method\":\"ping\"}\r\n\n{\"id\":4,\"method\":\"pong\"}\n").unwrap();
        let mut r = BufReader::new(c);
        for (id, method) in [(3, "ping"), (4, "pong")] {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!((v["id"].as_i64(), v["method"].as_str()), (Some(id), Some(method)), "{line}");
        }
    }

    /// Run `handle` over a fake stream; returns what it wrote and the methods it dispatched.
    fn run(input: &str) -> (String, Vec<String>) {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        handle(
            &mut input.as_bytes(),
            &mut out,
            TEST_TOKEN,
            |method, _| {
                seen.push(method);
                Some(json!({"ok": true}))
            },
            || {},
        );
        (String::from_utf8(out).unwrap(), seen)
    }

    #[test]
    fn the_auth_line_comes_first() {
        let (out, seen) = run(&format!("auth {TEST_TOKEN}\r\n{{\"id\":1,\"method\":\"ping\"}}\n"));
        assert_eq!(seen, ["ping"]);
        assert!(out.contains("\"id\":1"), "{out}");
    }

    #[test]
    fn a_request_before_auth_closes_without_reply() {
        let (out, seen) = run(&format!("{{\"id\":1,\"method\":\"ping\"}}\nauth {TEST_TOKEN}\n{{\"id\":2,\"method\":\"ping\"}}\n"));
        assert_eq!((out.as_str(), seen.len()), ("", 0));
        let (out, seen) = run("");
        assert_eq!((out.as_str(), seen.len()), ("", 0), "no first line");
        let (out, seen) = run("\nauth x\n");
        assert_eq!((out.as_str(), seen.len()), ("", 0), "a blank first line is not an auth line");
    }

    #[test]
    fn a_wrong_token_closes_without_reply() {
        for bad in ["auth nope", "auth ", "auth", &format!("auth {TEST_TOKEN}x"), &format!("AUTH {TEST_TOKEN}")] {
            let (out, seen) = run(&format!("{bad}\n{{\"id\":1,\"method\":\"ping\"}}\n"));
            assert_eq!((out.as_str(), seen.len()), ("", 0), "{bad}");
        }
    }

    #[test]
    fn a_parse_error_closes_the_connection() {
        let (out, seen) = run(&format!("auth {TEST_TOKEN}\nGET / HTTP/1.1\n{{\"id\":1,\"method\":\"ping\"}}\n"));
        assert_eq!(seen.len(), 0, "the request after the bad line is never served");
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("bad JSON"), "{out}");
    }

    #[test]
    fn a_wrong_token_over_tcp_is_closed() {
        let addr = serve_echo();
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"auth wrong\n{\"id\":1,\"method\":\"ping\"}\n").unwrap();
        let mut line = String::new();
        assert_eq!(BufReader::new(c).read_line(&mut line).unwrap_or(0), 0, "closed with no reply: {line:?}");
    }

    #[test]
    fn extra_connections_are_refused() {
        let addr = serve_echo();
        // Idle connections are accepted in order and held open.
        let held: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let mut extra = TcpStream::connect(addr).unwrap();
        extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut line = String::new();
        BufReader::new(&mut extra).read_line(&mut line).unwrap();
        assert!(line.contains("too many control connections"), "{line}");
        drop(held);
    }
}
