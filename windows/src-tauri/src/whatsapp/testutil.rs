// A tiny HTTP server for the tests: it plays GOWA (and, for the installer, GitHub). Plain std
// sockets on a thread of its own, so it needs no async runtime and works from any test.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::gateway::{parse_request, Parse};

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Without the query.
    pub path: String,
    #[allow(dead_code)]
    pub query: String,
    /// Lowercased names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// The server's own port, to build links to itself.
    pub port: u16,
}

impl Request {
    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
}

/// (status, headers, body)
pub type Reply = (u16, Vec<(String, String)>, Vec<u8>);

pub struct FakeServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl FakeServer {
    pub fn start<F>(handler: F) -> FakeServer
    where
        F: Fn(&Request) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        let (seen, halt) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (handler, seen) = (handler.clone(), seen.clone());
                std::thread::spawn(move || serve(stream, port, &*handler, &seen));
            }
        });
        FakeServer { port, requests, stop }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The requests received so far for `method` and `path`.
    pub fn seen(&self, method: &str, path: &str) -> Vec<Request> {
        self.requests.lock().unwrap().iter().filter(|r| r.method == method && r.path == path).cloned().collect()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut stream: TcpStream, port: u16, handler: &dyn Fn(&Request) -> Reply, seen: &Mutex<Vec<Request>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let request = loop {
        match parse_request(&data) {
            Parse::Done(r) => break r,
            Parse::Invalid => return,
            Parse::Incomplete => {}
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buffer[..n]),
        }
    };
    let (path, query) = match request.path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (request.path.clone(), String::new()),
    };
    let received = Request { method: request.method, path, query, headers: request.headers, body: request.body, port };
    seen.lock().unwrap().push(received.clone());
    let (status, headers, body) = handler(&received);
    let mut out = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
