// Network helpers shared by the chat providers: address checks, bounded reads
// and HTTP clients.
//
// Every answer a server sends is read with a ceiling, so a misbehaving or
// hostile server (a local model address can point anywhere) cannot make the
// app hold an unbounded amount of memory.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use reqwest::Url;

use crate::i18n::{t, tf};

/// Ceiling for a JSON answer: a chat reply or a model list.
pub const MAX_BODY: usize = 8 * 1024 * 1024;
/// Ceiling for an error body: only its message is shown.
pub const MAX_ERROR_BODY: usize = 64 * 1024;

/// True for an address that is this machine: `localhost`, 127.0.0.0/8, ::1,
/// and the unspecified addresses (0.0.0.0, ::), which connect to this machine.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

pub fn is_loopback_url(url: &Url) -> bool {
    url.host_str().is_some_and(is_loopback_host)
}

/// The address of a model server as the user typed or pasted it, cleaned up:
/// a missing scheme becomes `http://`, trailing slashes and the documentation
/// sub-paths people paste (`/api`, `/v1`) go, and the loopback names become
/// 127.0.0.1 — on Windows `localhost` may resolve to ::1 first while Ollama
/// only listens on IPv4. Only http and https, and no `user:password@`.
pub fn normalise_server_url(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(t("Enter the server address first."));
    }
    let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("http://{raw}") };
    let mut url = Url::parse(&with_scheme).map_err(|_| tf("Not a valid address: {address}", &[("address", raw)]))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(t("The address must start with http:// or https://."));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(t("Leave the user name and password out of the address."));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(tf("Not a valid address: {address}", &[("address", raw)]));
    }
    if is_loopback_url(&url) {
        let _ = url.set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    let mut path = url.path().trim_end_matches('/').to_string();
    for suffix in ["/api", "/v1"] {
        if let Some(rest) = path.strip_suffix(suffix) {
            path = rest.trim_end_matches('/').to_string();
        }
    }
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// `base` + `tail`, with exactly one slash between them.
pub fn join(base: &Url, tail: &str) -> String {
    format!("{}/{}", base.as_str().trim_end_matches('/'), tail.trim_start_matches('/'))
}

/// The Messages endpoint for an Anthropic-compatible gateway given in
/// COUCOU_ANTHROPIC_BASE_URL. A base URL (`https://gw.example.com`,
/// `…/v1`) gets `/v1/messages` added; a full endpoint is kept. The key goes
/// wherever this points, so it must be https — plain http only to this machine.
pub fn anthropic_endpoint(raw: &str) -> Result<Url, String> {
    const VAR: &str = "COUCOU_ANTHROPIC_BASE_URL";
    let mut url = Url::parse(raw.trim()).map_err(|_| format!("{VAR} is not a valid URL."))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_url(&url) => {}
        "http" => return Err(format!("{VAR} must use https:// (plain http only to this computer).")),
        _ => return Err(format!("{VAR} must start with https://.")),
    }
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return Err(format!("{VAR} must not carry a user name or password."));
    }
    let path = url.path().trim_end_matches('/').to_string();
    let path = if path.ends_with("/v1/messages") {
        path
    } else if path.ends_with("/v1") {
        format!("{path}/messages")
    } else {
        format!("{path}/v1/messages")
    };
    url.set_path(&path);
    url.set_fragment(None);
    Ok(url)
}

/// A client for `url`: requests to this machine skip any system proxy, which
/// would otherwise see (and usually fail) a loopback address.
pub fn client(url: &Url, timeout: Duration) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10).min(timeout))
        .timeout(timeout);
    if is_loopback_url(url) {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|e| e.to_string())
}

/// The body of `response`, refused past `limit` bytes.
pub async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(t("The server's answer is too large."));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| tf("Network error: {error}", &[("error", &e.to_string())]))? {
        if body.len() + chunk.len() > limit {
            return Err(t("The server's answer is too large."));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The text of an error body, cut down for a message: `error.message`
/// (OpenAI, Anthropic), a bare `error` string (Ollama), or the first characters.
pub fn error_detail(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(m) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
        if let Some(m) = v.get("error").and_then(|m| m.as_str()) {
            return m.to_string();
        }
        if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
        // Gemini answers some errors as a one-element array.
        if let Some(m) = v.pointer("/0/error/message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
    }
    text.trim().chars().take(200).collect()
}

/// Only the host of an address, for the log: never a path, a query or a key.
pub fn host_for_log(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => "?".into(),
    }
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ` (UTC), the form the Cloudflare and Hetzner
/// APIs take for a time range. Days to a calendar date by Howard Hinnant's civil_from_days.
pub fn iso_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised_and_others_are_not() {
        for host in ["localhost", "LOCALHOST", "app.localhost", "127.0.0.1", "127.8.9.1", "::1", "[::1]", "0.0.0.0", "[::]", "::ffff:127.0.0.1"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["example.com", "192.168.1.10", "10.0.0.2", "localhost.example.com", "[2001:db8::1]", "128.0.0.1", ""] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn a_pasted_server_address_is_cleaned_up() {
        let n = |s: &str| normalise_server_url(s).map(|u| u.to_string());
        assert_eq!(n("  http://localhost:11434/  ").unwrap(), "http://127.0.0.1:11434/");
        assert_eq!(n("http://localhost:11434/v1").unwrap(), "http://127.0.0.1:11434/");
        assert_eq!(n("http://127.0.0.1:1234/v1/").unwrap(), "http://127.0.0.1:1234/");
        assert_eq!(n("localhost:11434/api").unwrap(), "http://127.0.0.1:11434/");
        assert_eq!(n("0.0.0.0:11434").unwrap(), "http://127.0.0.1:11434/");
        assert_eq!(n("http://[::1]:8000").unwrap(), "http://127.0.0.1:8000/");
        assert_eq!(n("https://llm.example.com/proxy/v1?x=1#y").unwrap(), "https://llm.example.com/proxy");
        assert_eq!(n("gpu-box.lan:8000").unwrap(), "http://gpu-box.lan:8000/");
    }

    #[test]
    fn a_server_address_that_is_not_http_is_refused() {
        assert!(normalise_server_url("").is_err());
        assert!(normalise_server_url("   ").is_err());
        assert!(normalise_server_url("file:///etc/passwd").is_err());
        assert!(normalise_server_url("javascript://alert(1)").is_err());
        assert!(normalise_server_url("http://user:pw@example.com").is_err());
        assert!(normalise_server_url("http://").is_err());
    }

    #[test]
    fn join_puts_one_slash_between_base_and_path() {
        let base = normalise_server_url("http://127.0.0.1:11434").unwrap();
        assert_eq!(join(&base, "/v1/models"), "http://127.0.0.1:11434/v1/models");
        let base = Url::parse("https://api.openai.com/v1").unwrap();
        assert_eq!(join(&base, "chat/completions"), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn the_anthropic_gateway_is_taken_as_a_base_url_and_must_be_https() {
        let e = |s: &str| anthropic_endpoint(s).map(|u| u.to_string());
        assert_eq!(e("https://gw.example.com").unwrap(), "https://gw.example.com/v1/messages");
        assert_eq!(e("https://gw.example.com/").unwrap(), "https://gw.example.com/v1/messages");
        assert_eq!(e("https://gw.example.com/anthropic").unwrap(), "https://gw.example.com/anthropic/v1/messages");
        assert_eq!(e("https://gw.example.com/v1").unwrap(), "https://gw.example.com/v1/messages");
        assert_eq!(e("https://gw.example.com/v1/messages/").unwrap(), "https://gw.example.com/v1/messages");
        assert_eq!(e(" http://localhost:4000 ").unwrap(), "http://localhost:4000/v1/messages");
        assert_eq!(e("http://127.0.0.1:4000/v1").unwrap(), "http://127.0.0.1:4000/v1/messages");
        assert!(e("http://gw.example.com").is_err());
        assert!(e("ftp://gw.example.com").is_err());
        assert!(e("https://me:secret@gw.example.com").is_err());
        assert!(e("not a url").is_err());
    }

    #[test]
    fn error_details_come_from_the_usual_places() {
        assert_eq!(error_detail(br#"{"error":{"message":"bad key"}}"#), "bad key");
        assert_eq!(error_detail(br#"{"error":"model 'x' not found"}"#), "model 'x' not found");
        assert_eq!(error_detail(br#"[{"error":{"message":"quota"}}]"#), "quota");
        assert_eq!(error_detail(b"  plain text  "), "plain text");
        assert_eq!(error_detail("x".repeat(500).as_bytes()).len(), 200);
    }

    #[test]
    fn only_the_host_goes_to_the_log() {
        let url = Url::parse("https://gw.example.com:8443/secret/path?key=abc").unwrap();
        assert_eq!(host_for_log(&url), "gw.example.com:8443");
        let url = Url::parse("https://gw.example.com/v1/messages").unwrap();
        assert_eq!(host_for_log(&url), "gw.example.com");
    }

    /// One-shot HTTP server on a free port answering the next request with `body`.
    pub(crate) fn serve_once(status: &str, headers: &str, body: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let head = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // The whole request first, then the answer, then a polite close: closing with part of the
            // request unread makes Windows reset the connection, and a reset throws away whatever the
            // client has not read yet, which is a large answer cut short.
            let _ = read_request(&mut conn);
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(&body);
            let _ = conn.flush();
            let _ = conn.shutdown(std::net::Shutdown::Write);
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut sink = [0u8; 1024];
            while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
        });
        url
    }

    /// Reads one HTTP request from `conn`: its head, then as much body as the head announces.
    fn read_request(conn: &mut std::net::TcpStream) -> Option<String> {
        use std::io::Read;
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = conn.read(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let wanted = text[..end]
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if data.len() >= end + 4 + wanted {
                    return Some(text);
                }
            }
        }
    }

    fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
        let _turn = loopback_turn();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn times_are_written_in_utc_for_the_apis() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400 + 86_399), "2000-02-29T23:59:59Z"); // a leap day
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(4_102_444_800), "2100-01-01T00:00:00Z");
        assert!(now_secs() > 1_700_000_000);
    }

    /// Tests that open several real loopback connections take their turn here. Some antivirus web shields
    /// scan loopback HTTP and hold connections back when many are open at once, so such tests fail with
    /// "error sending request" or a timeout although the code is right; one at a time they take milliseconds.
    pub(crate) fn loopback_turn() -> std::sync::MutexGuard<'static, ()> {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A local HTTP server for the read-only dashboards' tests: it answers any number of requests, each
    /// from the first route whose `prefix` starts the request's "METHOD /path?query" (404 `{}` when none
    /// does). Returns its address and the raw requests it received (head and body), in order.
    pub(crate) fn serve_routes(routes: Vec<(&str, u16, String)>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        let routes: Arc<Vec<(String, u16, String)>> = Arc::new(routes.into_iter().map(|(p, s, b)| (p.to_string(), s, b)).collect());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let (routes, log) = (routes.clone(), log.clone());
                std::thread::spawn(move || {
                    let Some(request) = read_request(&mut conn) else { return };
                    let line = request.lines().next().unwrap_or("").trim_end_matches(" HTTP/1.1").to_string();
                    log.lock().unwrap().push(request);
                    let (status, body) = routes
                        .iter()
                        .find(|(prefix, _, _)| line.starts_with(prefix.as_str()))
                        .map(|(_, s, b)| (*s, b.clone()))
                        .unwrap_or((404, "{}".into()));
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = conn.write_all(head.as_bytes());
                    let _ = conn.write_all(body.as_bytes());
                    let _ = conn.flush();
                    let _ = conn.shutdown(std::net::Shutdown::Write);
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut sink = [0u8; 1024];
                    while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
                });
            }
        });
        (url, seen)
    }

    #[test]
    fn a_body_past_the_ceiling_is_refused_with_or_without_a_length() {
        let get = |url: String| async move {
            let url = Url::parse(&url).unwrap();
            let response = client(&url, Duration::from_secs(5)).unwrap().get(url).send().await.unwrap();
            read_capped(response, 1000).await
        };
        // Declared length too large.
        let url = serve_once("200 OK", "Content-Length: 5000\r\n", vec![b'x'; 5000]);
        assert!(block_on(get(url)).is_err());
        // No length (read to the end of the connection): stopped while reading.
        let url = serve_once("200 OK", "", vec![b'x'; 5000]);
        assert!(block_on(get(url)).is_err());
        // Under the ceiling: read whole.
        let url = serve_once("200 OK", "Content-Length: 3\r\n", b"abc".to_vec());
        assert_eq!(block_on(get(url)).unwrap(), b"abc");
    }
}
