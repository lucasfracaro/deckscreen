//! Tiny HTTP/1.1 server and client on std::net (one request per connection).

use crate::json::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

const MAX_BODY: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 64;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Raw target (path + "?" + query) exactly as sent; used for signatures.
    pub target: String,
    pub query: Vec<(String, String)>,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Result<Json, String> {
        if self.body.is_empty() {
            return Ok(Json::obj());
        }
        let s = std::str::from_utf8(&self.body).map_err(|_| "corpo não é UTF-8".to_string())?;
        Json::parse(s)
    }
    pub fn peer_ip(&self) -> String {
        let ip = self.peer.ip().to_string();
        ip.strip_prefix("::ffff:").map(str::to_string).unwrap_or(ip)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, v: &Json) -> Response {
        Response {
            status,
            body: v.to_compact(),
        }
    }
    pub fn error(status: u16, msg: &str) -> Response {
        Response::json(status, &crate::obj! {"error" => msg})
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 3 <= b.len() => {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &TcpStream, peer: SocketAddr) -> Result<Request, (u16, String)> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| (400, e.to_string()))?;
    let mut parts = line.trim_end().splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || !target.starts_with('/') {
        return Err((400, "requisição inválida".into()));
    }
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h).map_err(|e| (400, e.to_string()))?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if headers.len() >= MAX_HEADER_LINES {
            return Err((400, "cabeçalhos demais".into()));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err((413, "corpo grande demais".into()));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| (400, e.to_string()))?;

    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let query = qs
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(kv), String::new()),
        })
        .collect();
    let path = path.trim_end_matches('/').to_string();
    Ok(Request {
        method,
        path: if path.is_empty() { "/".into() } else { path },
        target,
        query,
        headers,
        body,
        peer,
    })
}

fn write_response(mut stream: &TcpStream, resp: &Response) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nServer: DeckScreen\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(resp.body.as_bytes());
    let _ = stream.flush();
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// Bind and serve forever on a background thread (thread per connection).
pub fn serve(addr: &str, handler: Handler) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::Builder::new()
        .name(format!("http-{addr}"))
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let handler = handler.clone();
                let _ = std::thread::Builder::new().name("http-conn".into()).spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                    let peer = stream.peer_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                    let resp = match read_request(&stream, peer) {
                        Ok(req) => handler(&req),
                        Err((code, msg)) => Response::error(code, &msg),
                    };
                    write_response(&stream, &resp);
                });
            }
        })?;
    Ok(local)
}

/// Plain HTTP client (used for the local control socket and tests).
pub fn request(
    method: &str,
    host_port: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let addr = host_port
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "endereço inválido".to_string())?;
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("resposta HTTP inválida")?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("status HTTP inválido")?;
    Ok((status, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode() {
        assert_eq!(url_decode("a%20b+c%2F"), "a b c/");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn server_roundtrip() {
        let h: Handler = Arc::new(|r: &Request| {
            Response::json(
                200,
                &crate::obj! {
                    "m" => r.method.clone(), "p" => r.path.clone(), "q" => r.query("x").unwrap_or(""),
                    "t" => r.target.clone(), "h" => r.header("X-Test").unwrap_or(""),
                    "b" => r.json().map(|j| j.str_at("k")).unwrap_or_default(),
                },
            )
        });
        let addr = serve("127.0.0.1:0", h).unwrap();
        let (st, body) = request(
            "POST",
            &addr.to_string(),
            "/api/a/?x=1%202",
            &[("X-Test", "ok")],
            Some(r#"{"k":"v"}"#),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(st, 200);
        let j = Json::parse(&body).unwrap();
        assert_eq!(j.str_at("p"), "/api/a");
        assert_eq!(j.str_at("q"), "1 2");
        assert_eq!(j.str_at("t"), "/api/a/?x=1%202");
        assert_eq!(j.str_at("h"), "ok");
        assert_eq!(j.str_at("b"), "v");
    }
}
