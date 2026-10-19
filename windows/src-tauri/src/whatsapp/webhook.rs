// The local door GOWA knocks on with each new message: an HTTP listener bound to 127.0.0.1 only
// (never reachable from another machine), on the port set in Settings. Each request is handed
// over whole; nothing of it is logged. Port of WhatsAppWebhook.swift.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::gateway::{parse_request, response, HttpRequest, Parse};

/// Called with each complete request; returns the HTTP status to answer.
pub type OnRequest = Arc<dyn Fn(HttpRequest) -> u16 + Send + Sync>;

/// Binds the loopback only; Err when the port is taken.
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}

/// Serves until the task is dropped or aborted.
pub async fn serve(listener: TcpListener, on_request: OnRequest) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        // Only the loopback: the listener is bound to it, this is a second lock.
        if !peer.ip().is_loopback() {
            continue;
        }
        let on_request = on_request.clone();
        tokio::spawn(async move {
            // A request that never finishes is dropped.
            let _ = tokio::time::timeout(Duration::from_secs(10), handle(stream, on_request)).await;
        });
    }
}

async fn handle(mut stream: TcpStream, on_request: OnRequest) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let answer = loop {
        match parse_request(&data) {
            Parse::Done(request) => {
                let status = on_request(request);
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    _ => "Error",
                };
                break response(status, reason);
            }
            Parse::Invalid => break response(400, "Bad Request"),
            Parse::Incomplete => {}
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break response(400, "Bad Request"),
            Ok(n) => data.extend_from_slice(&buffer[..n]),
        }
    };
    let _ = stream.write_all(&answer).await;
    let _ = stream.shutdown().await;
}
