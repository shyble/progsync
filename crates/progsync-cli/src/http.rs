//! HTTP API served by the watcher daemon (`psync daemon start --http`).
//!
//! The same tools as the MCP server, over plain HTTP/1.1 with JSON:
//!
//! - `GET  /v1/health`             -> `{"ok": true}` (no token needed)
//! - `GET  /v1/tools`              -> the tool list (name, description, schema)
//! - `POST /v1/tools/<name>`       -> run a tool; body: a JSON object of arguments
//! - `GET  /v1/tools/<name>?k=v`   -> run a read-only tool with query arguments
//!
//! Results are the command's `--json` output. Status: 200 on success, 422 when
//! the command reports `"ok": false`, 400/401/404/405/413 for bad requests.
//! Every request except health needs `Authorization: Bearer <token>`; the URL
//! and token are in `<workspace dir>/http.json` (mode 0600) while the daemon
//! runs. `X-Psync-Agent` sets the agent for one request.

use crate::mcp;
use progsync_core::bail;
use progsync_core::error::{Context, Result};
use progsync_core::json;
use progsync_core::json::{Map, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_BODY: usize = 8 << 20;

pub fn info_path(ws_dir: &Path) -> PathBuf {
    ws_dir.join("http.json")
}

struct Ctx {
    root: PathBuf,
    agent: Option<String>,
    token: String,
}

/// Bind `addr`, publish the URL and token, and serve in background threads.
/// Returns the URL.
pub fn start(addr: &str, root: PathBuf, ws_dir: &Path, agent: Option<String>) -> Result<String> {
    let listener = TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr()?;
    let url = format!("http://{local}");
    let token = progsync_core::util::random_hex(32);
    let info = info_path(ws_dir);
    let body = progsync_core::json::to_vec_pretty(&json!({ "url": url, "token": token, "pid": std::process::id() }))?;
    let tmp = ws_dir.join(format!("http.json.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    progsync_core::platform::restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, &info)?;
    let ctx = Arc::new(Ctx { root, agent, token });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &ctx);
            });
        }
    });
    Ok(url)
}

/// Remove the published URL and token (when the daemon exits).
pub fn stop(ws_dir: &Path) {
    let _ = std::fs::remove_file(info_path(ws_dir));
}

/// The published URL of a running daemon's HTTP API, if any.
pub fn url(ws_dir: &Path) -> Option<String> {
    let v: Value = progsync_core::json::from_slice(&std::fs::read(info_path(ws_dir)).ok()?).ok()?;
    v.get("url")?.as_str().map(String::from)
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &TcpStream) -> Result<std::result::Result<Request, (u16, String)>> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(Err((400, "malformed request line".into())));
    };
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        if headers.len() > 100 {
            return Ok(Err((400, "too many headers".into())));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(Err((413, "request body too large".into())));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    Ok(Ok(Request { method: method.to_string(), path: decode(path), query, headers, body }))
}

/// Percent-decoding (and `+` as space) for URL parts.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        _ => "Error",
    };
    let text = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn error(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "ok": false, "error": msg }))
}

fn handle(mut stream: TcpStream, ctx: &Ctx) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let (status, body) = match read_request(&stream)? {
        Err((s, m)) => error(s, &m),
        Ok(req) => route(&req, ctx),
    };
    respond(&mut stream, status, &body)
}

fn route(req: &Request, ctx: &Ctx) -> (u16, Value) {
    if req.path == "/v1/health" {
        return (200, json!({ "ok": true }));
    }
    let authorized = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).is_some_and(|t| {
        // Constant-time comparison.
        t.len() == ctx.token.len() && t.bytes().zip(ctx.token.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
    });
    if !authorized {
        return error(401, "missing or wrong token: send `Authorization: Bearer <token>` (see http.json in the workspace's .psync directory)");
    }
    if req.path == "/v1/tools" {
        return match req.method.as_str() {
            "GET" => (200, json!({ "ok": true, "tools": mcp::tool_list() })),
            _ => error(405, "use GET"),
        };
    }
    let Some(name) = req.path.strip_prefix("/v1/tools/") else { return error(404, "not found; see GET /v1/tools") };
    let Some(read_only) = mcp::is_read_only(name) else { return error(404, &format!("unknown tool '{name}'")) };
    let args = match req.method.as_str() {
        "POST" => match parse_body(&req.body) {
            Ok(a) => a,
            Err(e) => return error(400, &format!("{e:#}")),
        },
        "GET" if read_only => req.query.iter().map(|(k, v)| (k.clone(), mcp::query_value(name, k, v))).collect(),
        "GET" => return error(405, &format!("'{name}' changes the repository: use POST")),
        _ => return error(405, "use GET or POST"),
    };
    let agent = req.header("x-psync-agent").map(String::from).or_else(|| ctx.agent.clone());
    match mcp::call_tool(Some(&ctx.root), agent.as_deref(), name, &args) {
        Some((v, false)) => (200, v),
        Some((v, true)) => (422, v),
        None => error(404, &format!("unknown tool '{name}'")),
    }
}

fn parse_body(body: &[u8]) -> Result<Map> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match progsync_core::json::from_slice(body).context("body must be a JSON object")? {
        Value::Object(m) => Ok(m),
        _ => bail!("body must be a JSON object of arguments"),
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn percent_decoding() {
        assert_eq!(decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
    }
}
