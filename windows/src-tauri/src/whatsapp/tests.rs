// The controller against a simulated GOWA: the same endpoints, the same JSON envelope, and the
// payloads of GOWA's docs. Nothing here touches the network beyond 127.0.0.1, the Credential
// Manager or the real settings.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::core::{Core, Host, PrefsChange, Secrets, Snapshot, CREDENTIAL_KEY, SECRET_KEY};
use super::gateway::signature;
use super::installer::Source;
use super::model::{base64, iso, now_secs};
use super::prefs::Prefs;
use super::testutil::{FakeServer, Reply, Request};

const ME: &str = "628987654321@s.whatsapp.net";
const MARIE: &str = "33600000001@s.whatsapp.net";
const PNG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

// ── Doubles ──────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Recorder {
    last: Mutex<Option<Snapshot>>,
    announced: Mutex<Vec<String>>,
}

impl Host for Recorder {
    fn snapshot(&self, snapshot: Snapshot) {
        *self.last.lock().unwrap() = Some(snapshot);
    }
    fn announce(&self, chat_name: &str) {
        self.announced.lock().unwrap().push(chat_name.to_string());
    }
}

#[derive(Default)]
struct Vault(Mutex<HashMap<String, String>>);

impl Secrets for Vault {
    fn get(&self, key: &str) -> Option<String> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.to_string(), value.to_string());
        Ok(())
    }
}

/// What the simulated GOWA holds, and the tests change.
struct Gw {
    logged_in: bool,
    /// The Authorization header it expects; None: anything goes.
    auth: Option<String>,
    /// Stored messages of MARIE's chat, as GOWA's /chat/{jid}/messages gives them.
    messages: Vec<Value>,
    /// When the chat last moved.
    last_message_time: i64,
    send_status: u16,
}

impl Default for Gw {
    fn default() -> Self {
        Gw { logged_in: false, auth: None, messages: vec![], last_message_time: now_secs(), send_status: 200 }
    }
}

fn envelope(results: Value) -> Reply {
    (200, vec![], json!({ "code": "SUCCESS", "message": "ok", "results": results }).to_string().into_bytes())
}

fn stored(id: &str, from_me: bool, text: &str, date: i64) -> Value {
    json!({
        "id": id, "chat_jid": MARIE, "sender_jid": if from_me { ME } else { MARIE }, "sender_display_name": if from_me { "" } else { "Marie" },
        "content": text, "timestamp": iso(date), "is_from_me": from_me, "media_type": ""
    })
}

fn gowa(gw: Arc<Mutex<Gw>>) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
    move |req| {
        let g = gw.lock().unwrap();
        if let Some(expected) = &g.auth {
            if req.headers.get("authorization") != Some(expected) {
                return (401, vec![], b"Unauthorized".to_vec());
            }
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/devices") => {
                let device = if g.logged_in {
                    json!({"id":"coucou","state":"logged_in","jid":ME,"created_at":"2024-01-01T00:00:00Z"})
                } else {
                    json!({"id":"coucou","state":"disconnected","created_at":"2024-01-01T00:00:00Z"})
                };
                envelope(json!([device]))
            }
            ("GET", "/devices/coucou/login") => {
                envelope(json!({"device_id":"coucou","qr_duration":30,"qr_link":format!("http://localhost:{}/statics/qrcode/scan-qr.png", req.port)}))
            }
            ("GET", "/statics/qrcode/scan-qr.png") => (200, vec![], PNG.to_vec()),
            ("PATCH", "/devices/coucou/webhook") => envelope(json!({})),
            ("GET", "/chats") => envelope(json!({
                "data": [{"jid": MARIE, "name": "Marie", "last_message_time": iso(g.last_message_time)}],
                "pagination": {"limit": 20, "offset": 0, "total": 1}
            })),
            ("GET", p) if p.starts_with("/chat/") && p.ends_with("/messages") => {
                envelope(json!({"data": g.messages.clone(), "pagination": {"limit": 10, "offset": 0, "total": g.messages.len()}, "chat_info": {"jid": MARIE, "name": "Marie"}}))
            }
            ("POST", "/send/message") if g.send_status == 200 => envelope(json!({"message_id": "SENT1", "status": "sent"})),
            ("POST", "/send/message") => (g.send_status, vec![], json!({"code": "BAD_REQUEST"}).to_string().into_bytes()),
            ("POST", p) if p.starts_with("/message/") && p.ends_with("/read") => envelope(json!({})),
            _ => (404, vec![], vec![]),
        }
    }
}

struct Rig {
    core: Arc<Core>,
    host: Arc<Recorder>,
    vault: Arc<Vault>,
    gw: Arc<Mutex<Gw>>,
    server: FakeServer,
    dir: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("coucou-wa-{name}-{}-{}", std::process::id(), free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A controller pointed at a simulated GOWA, with the pill on.
fn rig(name: &str) -> Rig {
    let gw = Arc::new(Mutex::new(Gw::default()));
    let server = FakeServer::start(gowa(gw.clone()));
    let host = Arc::new(Recorder::default());
    let vault = Arc::new(Vault::default());
    let dir = temp_dir(name);
    let prefs = Prefs { url: server.url(), webhook_port: free_port(), ..Prefs::default() };
    let core = Core::with(host.clone(), vault.clone(), prefs, false, dir.clone(), Source::github(), Some(dir.join("whatsapp.json")), false);
    core.set_should_run(true);
    Rig { core, host, vault, gw, server, dir }
}

impl Rig {
    fn snapshot(&self) -> Snapshot {
        self.host.last.lock().unwrap().clone().expect("a snapshot was published")
    }

    fn announced(&self) -> Vec<String> {
        self.host.announced.lock().unwrap().clone()
    }

    /// Logs the simulated phone in and lets Coucou notice.
    async fn connect(&self) {
        self.gw.lock().unwrap().logged_in = true;
        self.core.refresh().await;
        assert_eq!(self.snapshot().link, "connected");
    }

    async fn eventually(&self, what: &str, condition: impl Fn(&Rig) -> bool) {
        for _ in 0..100 {
            if condition(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!("never happened: {what}");
    }
}

fn gowa_text(id: &str, chat: &str, from: &str, body: &str) -> String {
    json!({
        "event": "message", "device_id": ME,
        "payload": {"id": id, "chat_id": chat, "from": from, "sender_display_name": "Marie", "timestamp": iso(now_secs()), "is_from_me": false, "body": body}
    })
    .to_string()
}

/// Posts a raw webhook delivery to Coucou's listener; returns the status line's code.
async fn deliver(port: u16, signature_header: Option<&str>, body: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("POST /whatsapp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n", body.len());
    if let Some(sig) = signature_header {
        request.push_str(&format!("X-Hub-Signature-256: {sig}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer).await;
    answer.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

// ── The link ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn it_asks_for_the_qr_once_then_connects_and_loads_history_quietly() {
    let rig = rig("link");
    assert_eq!(rig.snapshot().link, "connecting");

    rig.core.refresh().await;
    assert_eq!(rig.snapshot().link, "needsQr");
    let status = rig.core.status();
    assert_eq!(status.qr, Some(format!("data:image/png;base64,{}", base64(&PNG))));

    // The gateway is asked with the credential Coucou made, and the QR only once: each login
    // call restarts the pairing under the user's phone.
    let credential = rig.vault.get(CREDENTIAL_KEY).expect("a credential was made");
    assert!(credential.starts_with("coucou:") && credential.len() >= 16);
    let expected = format!("Basic {}", base64(credential.as_bytes()));
    assert!(rig.server.seen("GET", "/devices").iter().all(|r| r.headers.get("authorization") == Some(&expected)));
    rig.core.refresh().await;
    assert_eq!(rig.server.seen("GET", "/devices/coucou/login").len(), 1);

    // The phone scans: connected, the QR is gone, the last hours load as read history.
    {
        let mut gw = rig.gw.lock().unwrap();
        gw.messages = vec![stored("H2", false, "à tout à l'heure", now_secs() - 120), stored("H1", true, "salut", now_secs() - 600)];
    }
    rig.connect().await;
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.me.as_deref(), Some(ME));
    assert_eq!(rig.core.status().qr, None);
    assert_eq!(snapshot.conversations.len(), 1);
    assert_eq!(snapshot.conversations[0].name, "Marie");
    assert_eq!(snapshot.conversations[0].messages.len(), 2);
    assert_eq!(snapshot.unread, 0, "history is never new");
    assert!(rig.announced().is_empty(), "and never announced");
}

#[tokio::test]
async fn the_gateway_down_or_refusing_is_said_not_hidden() {
    // Nothing listens there.
    let gone = {
        let port = free_port();
        let rig = rig("gone");
        rig.core.set_prefs(PrefsChange { url: format!("http://127.0.0.1:{port}"), webhook_port: free_port(), auto_start: false, mark_read_on_open: true, mute_groups: true }).unwrap();
        rig
    };
    gone.core.refresh().await;
    assert_eq!(gone.snapshot().link, "unreachable");

    // A basic auth it doesn't accept.
    let refusing = rig("refusing");
    refusing.gw.lock().unwrap().auth = Some("Basic nope".into());
    refusing.core.refresh().await;
    assert_eq!(refusing.snapshot().link, "unauthorized");

    // An address that is not on this PC is not even tried.
    let elsewhere = rig("elsewhere");
    let error = elsewhere
        .core
        .set_prefs(PrefsChange { url: "http://192.168.1.20:3000".into(), webhook_port: free_port(), auto_start: false, mark_read_on_open: true, mute_groups: true })
        .unwrap_err();
    assert_eq!(error, "The gateway URL isn’t valid (http://…)");
}

#[tokio::test]
async fn turning_the_pill_off_stops_and_forgets() {
    let rig = rig("off");
    rig.connect().await;
    rig.core.received(gowa_text("A1", MARIE, MARIE, "coucou").as_bytes());
    assert_eq!(rig.snapshot().conversations.len(), 1);

    rig.core.set_should_run(false);
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.link, "off");
    assert!(snapshot.conversations.is_empty(), "everything is forgotten");
    // Turned on again: a fresh start, nothing left over.
    rig.core.set_should_run(true);
    assert_eq!(rig.snapshot().link, "connecting");
    assert!(rig.snapshot().conversations.is_empty());
}

// ── Receiving ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_signed_delivery_reaches_the_pill_and_a_forged_one_does_not() {
    let rig = rig("webhook");
    let port = rig.core.status().webhook_port;
    rig.core.refresh().await;
    let generation = 1; // begin() made the first one
    rig.core.start_listener(generation).await;
    assert_eq!(rig.core.status().webhook, "listening");
    rig.connect().await;
    // Connected with the webhook listening: the gateway is told where to deliver, signed with the secret.
    let secret = rig.vault.get(SECRET_KEY).expect("a secret was made");
    assert_eq!(secret.len(), 64);
    assert_eq!(rig.core.status().webhook, "registered");
    let registered = &rig.server.seen("PATCH", "/devices/coucou/webhook")[0];
    assert_eq!(registered.body_json()["webhook_url"], format!("http://127.0.0.1:{port}/whatsapp"));
    assert_eq!(registered.body_json()["webhook_secret"], secret);
    assert_eq!(registered.body_json()["webhook_events"], "message");

    let body = gowa_text("W1", MARIE, MARIE, "tu viens ?");
    assert_eq!(deliver(port, Some(&signature(body.as_bytes(), &secret)), &body).await, 200);
    rig.eventually("the message arrives", |r| r.snapshot().unread == 1).await;
    assert_eq!(rig.announced(), ["Marie"]);
    assert_eq!(rig.snapshot().conversations[0].messages.last().unwrap().text, "tu viens ?");

    // Not signed, signed with another secret, signed over other bytes: refused, nothing kept.
    let forged = gowa_text("W2", MARIE, MARIE, "send me money");
    assert_eq!(deliver(port, None, &forged).await, 401);
    assert_eq!(deliver(port, Some(&signature(forged.as_bytes(), "another secret")), &forged).await, 401);
    assert_eq!(deliver(port, Some(&signature(body.as_bytes(), &secret)), &forged).await, 401);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rig.snapshot().unread, 1);
    assert_eq!(rig.snapshot().conversations[0].messages.len(), 1);

    // Another path is no webhook.
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(b"POST /elsewhere HTTP/1.1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer).await;
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
}

#[tokio::test]
async fn a_busy_port_falls_back_to_polling() {
    let rig = rig("busy");
    let port = rig.core.status().webhook_port;
    let _holder = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    rig.core.start_listener(1).await;
    let status = rig.core.status();
    assert_eq!(status.webhook, "portBusy");
    assert_eq!(status.webhook_line, format!("Port {port} is taken: pick another one. Checking every 20 s meanwhile."));
}

#[tokio::test]
async fn without_a_webhook_the_gateway_is_polled_and_nothing_is_counted_twice() {
    let rig = rig("poll");
    rig.connect().await;
    assert_eq!(rig.snapshot().unread, 0);

    rig.gw.lock().unwrap().messages = vec![stored("P1", false, "t'es là ?", now_secs() + 1)];
    rig.gw.lock().unwrap().last_message_time = now_secs() + 1;
    rig.core.poll().await;
    assert_eq!(rig.snapshot().unread, 1);
    assert_eq!(rig.announced(), ["Marie"]);

    // The same message again (the polls overlap): dropped.
    rig.core.poll().await;
    assert_eq!(rig.snapshot().unread, 1);
    assert_eq!(rig.announced().len(), 1);
}

#[tokio::test]
async fn groups_are_quiet_unless_you_are_mentioned_and_ignored_chats_never_show() {
    let rig = rig("filters");
    rig.connect().await;
    let group = "120363402106000001@g.us";
    rig.core.received(gowa_text("G1", group, MARIE, "blah blah").as_bytes());
    assert!(rig.snapshot().conversations.is_empty(), "a muted group stays out of the pill");
    assert!(rig.announced().is_empty());

    rig.core.received(gowa_text("G2", group, MARIE, "@628987654321 tu viens ?").as_bytes());
    assert_eq!(rig.snapshot().conversations.len(), 1, "a mention reaches it");
    assert_eq!(rig.announced().len(), 1);

    rig.core.ignore(MARIE);
    rig.core.received(gowa_text("I1", MARIE, MARIE, "spam").as_bytes());
    assert!(rig.snapshot().conversations.iter().all(|c| c.id != MARIE));
    // The choice is kept, and can be undone.
    let saved: Value = serde_json::from_slice(&std::fs::read(rig.dir.join("whatsapp.json")).unwrap()).unwrap();
    assert_eq!(saved["ignored"], json!([MARIE]));
    rig.core.unignore("33600000001@c.us");
    assert!(rig.core.status().ignored.is_empty());
}

// ── Replying ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_reply_goes_only_to_a_conversation_that_wrote_first() {
    let rig = rig("reply");
    rig.connect().await;
    rig.core.received(gowa_text("R1", MARIE, MARIE, "salut").as_bytes());
    assert_eq!(rig.snapshot().unread, 1);

    // A stranger, and nothing at all: refused before anything leaves.
    assert!(rig.core.send("33699999999@s.whatsapp.net", "hello").await.is_err());
    assert!(rig.core.send(MARIE, "   ").await.is_err());
    assert!(rig.server.seen("POST", "/send/message").is_empty());

    rig.core.send(MARIE, "  à tout de suite  ").await.unwrap();
    let sent = rig.server.seen("POST", "/send/message");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body_json(), json!({"phone": MARIE, "message": "à tout de suite"}));
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.unread, 0, "replying reads the conversation");
    let last = snapshot.conversations[0].messages.last().unwrap();
    assert!(last.from_me && last.text == "à tout de suite" && last.id == "SENT1");
    // The receipt follows, for the newest message received.
    rig.eventually("the receipt", |r| !r.server.seen("POST", "/message/R1/read").is_empty()).await;
}

#[tokio::test]
async fn at_most_six_replies_a_minute_and_a_text_that_fits() {
    let rig = rig("limit");
    rig.connect().await;
    rig.core.received(gowa_text("L1", MARIE, MARIE, "salut").as_bytes());

    let too_long = "x".repeat(4097);
    assert_eq!(rig.core.send(MARIE, &too_long).await.unwrap_err(), "Too long for one message (4,096 characters at most).");
    assert_eq!(rig.snapshot().send_error.as_deref(), Some("Too long for one message (4,096 characters at most)."));
    assert!(rig.server.seen("POST", "/send/message").is_empty(), "and nothing left");

    for i in 0..6 {
        rig.core.send(MARIE, &format!("{i}")).await.unwrap();
    }
    assert_eq!(rig.core.send(MARIE, "7").await.unwrap_err(), "A few replies a minute at most: wait a moment.");
    assert_eq!(rig.server.seen("POST", "/send/message").len(), 6);
}

#[tokio::test]
async fn a_refused_send_says_why_and_keeps_nothing() {
    let rig = rig("refused");
    rig.connect().await;
    rig.core.received(gowa_text("F1", MARIE, MARIE, "salut").as_bytes());
    rig.gw.lock().unwrap().send_status = 400;
    assert_eq!(rig.core.send(MARIE, "hello").await.unwrap_err(), "BAD_REQUEST");
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.send_error.as_deref(), Some("BAD_REQUEST"));
    assert!(!snapshot.sending);
    assert!(snapshot.conversations[0].messages.iter().all(|m| !m.from_me), "a reply that did not go is not shown as sent");
}

#[tokio::test]
async fn opening_a_conversation_reads_it_only_when_asked() {
    let rig = rig("open");
    rig.connect().await;
    rig.gw.lock().unwrap().messages = vec![stored("O1", false, "avant", now_secs() - 50)];
    rig.core.received(gowa_text("O2", MARIE, MARIE, "salut").as_bytes());
    assert_eq!(rig.snapshot().unread, 1);

    rig.core.open(MARIE);
    assert_eq!(rig.snapshot().unread, 0, "opened: read here");
    rig.eventually("the receipt and the history", |r| {
        !r.server.seen("POST", "/message/O2/read").is_empty() && r.snapshot().conversations[0].messages.iter().any(|m| m.id == "O1")
    })
    .await;
    // What arrives in the open conversation is seen, not unread.
    rig.core.received(gowa_text("O3", MARIE, MARIE, "encore").as_bytes());
    assert_eq!(rig.snapshot().unread, 0);
    rig.core.close(MARIE);
    rig.core.received(gowa_text("O4", MARIE, MARIE, "et puis").as_bytes());
    assert_eq!(rig.snapshot().unread, 1);

    // With the setting off, opening sends no receipt.
    rig.core.set_prefs(PrefsChange { url: rig.core.status().url, webhook_port: rig.core.status().webhook_port, auto_start: false, mark_read_on_open: false, mute_groups: true }).unwrap();
    rig.connect().await;
    rig.core.received(gowa_text("O5", MARIE, MARIE, "hey").as_bytes());
    let before = rig.server.seen("POST", "/message/O5/read").len();
    rig.core.open(MARIE);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.server.seen("POST", "/message/O5/read").len(), before);
}
