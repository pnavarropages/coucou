// What GOWA sends and answers, turned into `Message`s. Pure parsing, so the tests check it against
// the payloads printed in GOWA's own docs. Every field name here comes from those docs and the
// gateway's source:
//   github.com/aldinokemal/go-whatsapp-web-multidevice (docs/webhook-payload.md, docs/openapi.yaml)
// Nothing parsed here is logged or written anywhere.

use std::collections::HashMap;

use ring::hmac;
use serde_json::Value;

use super::model::{self, is_group, parse_rfc3339, text_mentions, Media, Message};

/// The header carrying the webhook's HMAC-SHA256 ("sha256=<hex>"), lowercased.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";

/// What one webhook delivery holds for Coucou.
#[derive(Debug, PartialEq)]
pub enum WebhookEvent {
    Message(Message),
    /// A valid delivery Coucou has no use for (receipts, reactions, a test ping…).
    Ignored,
}

/// What GOWA says of the WhatsApp link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// Logged in and online.
    Connected,
    /// Waiting for the QR to be scanned.
    NeedsQr,
    /// Logged in but offline, starting, or anything in between.
    Connecting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub link: Link,
    /// This account's JID or number, once logged in.
    pub me: Option<String>,
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

// ── Webhook ──────────────────────────────────────────────────────────────────

/// "sha256=<lowercase hex>" of HMAC-SHA256(secret, body): what GOWA sends.
#[cfg(test)]
pub fn signature(body: &[u8], secret: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    format!("sha256={}", model::hex(hmac::sign(&key, body).as_ref()))
}

/// True when the delivery carries the signature `secret` gives, over the raw body bytes.
/// `headers` has lowercased names.
pub fn verify(headers: &HashMap<String, String>, body: &[u8], secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(sent) = headers.get(SIGNATURE_HEADER) else { return false };
    let Some(hex) = sent.strip_prefix("sha256=") else { return false };
    let Some(tag) = model::unhex(hex) else { return false };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    // ring compares in constant time.
    hmac::verify(&key, body, &tag).is_ok()
}

/// Parses one delivery. None when it isn't JSON at all. `me` is this account's JID or number,
/// to tell a mention of you (GOWA also gives it in the delivery itself).
///
/// `{"event":"message","device_id":"<own JID>","payload":{id, chat_id, from, from_name,
/// sender_display_name, timestamp (RFC3339), is_from_me, body, image|video|audio|document|…}}`
pub fn parse_webhook(body: &[u8], me: Option<&str>) -> Option<WebhookEvent> {
    let root = json(body)?;
    let root = root.as_object()?;
    if root.get("event").and_then(Value::as_str) != Some("message") {
        return Some(WebhookEvent::Ignored);
    }
    let Some(p) = root.get("payload").and_then(Value::as_object) else { return Some(WebhookEvent::Ignored) };
    let (Some(id), Some(chat)) = (non_empty(p.get("id")), non_empty(p.get("chat_id"))) else {
        return Some(WebhookEvent::Ignored);
    };
    let from = p.get("from").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| chat.clone());
    let text = p.get("body").and_then(Value::as_str).unwrap_or("").to_string();
    let device = root.get("device_id").and_then(Value::as_str);
    let mine = me.or(device);
    let mut message = Message::new(
        &id,
        &chat,
        &from,
        &non_empty(p.get("sender_display_name")).or_else(|| non_empty(p.get("from_name"))).unwrap_or_default(),
        &text,
        p.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or_else(model::now_secs),
    );
    message.media = webhook_media(p);
    message.from_me = p.get("is_from_me").and_then(Value::as_bool).unwrap_or(false);
    message.mentions_me = is_group(&chat) && text_mentions(&text, mine);
    Some(WebhookEvent::Message(message))
}

/// GOWA puts each media under its own key; only the label matters to Coucou.
fn webhook_media(p: &serde_json::Map<String, Value>) -> Media {
    let has = |k: &str| p.get(k).is_some_and(|v| !v.is_null());
    if has("image") {
        Media::Image
    } else if has("video") || has("video_note") {
        Media::Video
    } else if has("audio") {
        Media::Voice { seconds: None }
    } else if let Some(doc) = p.get("document").filter(|v| !v.is_null()) {
        Media::Document { name: non_empty(doc.get("filename")) }
    } else if has("sticker") {
        Media::Sticker
    } else if has("location") || has("live_location") {
        Media::Location
    } else if has("contact") || has("contacts_array") {
        Media::Contact
    } else if has("poll") {
        Media::Other
    } else {
        Media::None
    }
}

// ── REST answers ─────────────────────────────────────────────────────────────

/// GOWA's envelope: `{code, message, results}` (no "status": the HTTP code says it).
pub fn results(body: &[u8]) -> Option<Value> {
    let mut root = json(body)?;
    root.get_mut("results").map(Value::take)
}

/// `GET /devices` → `results: [{id, phone_number, display_name, state, jid, created_at}]`.
/// The device Coucou uses: a logged-in one first, else the first one.
pub fn device(body: &[u8]) -> Option<(String, Status)> {
    let list = results(body)?;
    let list = list.as_array().filter(|l| !l.is_empty())?;
    let pick = list.iter().find(|d| d.get("state").and_then(Value::as_str) == Some("logged_in")).unwrap_or(&list[0]);
    let id = non_empty(pick.get("id"))?;
    let jid = non_empty(pick.get("jid"));
    let link = match pick.get("state").and_then(Value::as_str) {
        Some("logged_in") => Link::Connected,
        _ if jid.is_none() => Link::NeedsQr,
        _ => Link::Connecting,
    };
    Some((id, Status { link, me: jid }))
}

/// `GET /devices/{id}/login` → `results: {device_id, qr_link, qr_duration}`. The PNG's path,
/// to fetch from the gateway itself (never from another host the link may name), and its life in
/// seconds.
pub fn qr_path(body: &[u8]) -> Option<(String, u64)> {
    let r = results(body)?;
    let link = r.get("qr_link")?.as_str()?;
    let url = reqwest::Url::parse(link).ok()?;
    if url.path().is_empty() || url.path() == "/" {
        return None;
    }
    Some((url.path().to_string(), r.get("qr_duration").and_then(Value::as_u64).unwrap_or(30)))
}

/// `./whatsapp rest` for Coucou starting GOWA: on this PC only, on the URL's port, and media
/// never downloaded (Coucou shows a label), so GOWA keeps no photo or voice note on disk.
pub fn launch_args(port: u16) -> Vec<String> {
    vec!["rest".into(), "--host=127.0.0.1".into(), format!("--port={port}"), "--auto-download-media=false".into()]
}

/// The error code of a refused call (`ALREADY_LOGGED_IN`, `DEVICE_ID_REQUIRED`…).
pub fn error_code(body: &[u8]) -> Option<String> {
    json(body)?.get("code")?.as_str().map(str::to_string)
}

pub struct ChatSummary {
    pub jid: String,
    pub name: String,
    pub last: i64,
}

/// `GET /chats` → `results: {data: [{jid, name, last_message_time, …}], pagination}`.
pub fn chats(body: &[u8]) -> Vec<ChatSummary> {
    let Some(r) = results(body) else { return Vec::new() };
    let Some(list) = r.get("data").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|c| {
            Some(ChatSummary {
                jid: non_empty(c.get("jid"))?,
                name: c.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                last: c.get("last_message_time").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(i64::MIN),
            })
        })
        .collect()
}

/// `GET /chat/{jid}/messages` → `results: {data: [{id, chat_jid, sender_jid, sender_display_name,
/// content, timestamp, is_from_me, media_type, filename}], chat_info}`. Newest first there.
pub fn messages(body: &[u8], me: Option<&str>) -> Vec<Message> {
    let Some(r) = results(body) else { return Vec::new() };
    let Some(list) = r.get("data").and_then(Value::as_array) else { return Vec::new() };
    let chat_name = non_empty(r.get("chat_info").and_then(|c| c.get("name")));
    list.iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            let chat = m.get("chat_jid")?.as_str()?;
            let text = m.get("content").and_then(Value::as_str).unwrap_or("");
            let mut msg = Message::new(
                id,
                chat,
                m.get("sender_jid").and_then(Value::as_str).unwrap_or(chat),
                &non_empty(m.get("sender_display_name")).unwrap_or_default(),
                text,
                m.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(0),
            );
            msg.media = stored_media(m.get("media_type").and_then(Value::as_str), non_empty(m.get("filename")));
            msg.from_me = m.get("is_from_me").and_then(Value::as_bool).unwrap_or(false);
            if is_group(chat) {
                msg.chat_name = chat_name.clone();
                msg.mentions_me = text_mentions(text, me);
            }
            Some(msg)
        })
        .collect()
}

/// `media_type` of a stored message: image|video|video_note|audio|document|sticker|call or "".
fn stored_media(kind: Option<&str>, filename: Option<String>) -> Media {
    match kind.unwrap_or("") {
        "" => Media::None,
        "image" => Media::Image,
        "video" | "video_note" => Media::Video,
        "audio" => Media::Voice { seconds: None },
        "document" => Media::Document { name: filename },
        "sticker" => Media::Sticker,
        _ => Media::Other,
    }
}

/// `POST /send/message` → `results: {message_id, status}`.
pub fn sent_id(body: &[u8]) -> Option<String> {
    non_empty(results(body)?.get("message_id"))
}

/// `GET /group/info` → `results: {Name, …}`.
pub fn group_name(body: &[u8]) -> Option<String> {
    non_empty(results(body)?.get("Name"))
}

// ── The webhook's HTTP request ───────────────────────────────────────────────

/// One HTTP request read from the local socket: enough of HTTP/1.1 for a gateway's webhook.
#[derive(Debug, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    /// Lowercased names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum Parse {
    Incomplete,
    Invalid,
    Done(HttpRequest),
}

const MAX_HEAD: usize = 16 * 1024;
pub const MAX_BODY: usize = 1_000_000;

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parses what arrived so far; `Incomplete` until the whole body is there.
pub fn parse_request(data: &[u8]) -> Parse {
    let Some(end) = find(data, b"\r\n\r\n") else {
        return if data.len() > MAX_HEAD { Parse::Invalid } else { Parse::Incomplete };
    };
    let Ok(head) = std::str::from_utf8(&data[..end]) else { return Parse::Invalid };
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ').filter(|p| !p.is_empty());
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else { return Parse::Invalid };
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: i64 = headers.get("content-length").map_or(Some(0), |v| v.parse().ok()).unwrap_or(-1);
    if length < 0 || length as usize > MAX_BODY {
        return Parse::Invalid;
    }
    let start = end + 4;
    if data.len() - start < length as usize {
        return Parse::Incomplete;
    }
    Parse::Done(HttpRequest {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body: data[start..start + length as usize].to_vec(),
    })
}

pub fn response(status: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    // GOWA docs/webhook-payload.md — text message
    const TEXT: &str = r#"{
      "event": "message",
      "device_id": "628987654321@s.whatsapp.net",
      "payload": {
        "id": "3EB0C127D7BACC83D6A1",
        "chat_id": "628987654321@s.whatsapp.net",
        "from": "628123456789@s.whatsapp.net",
        "from_lid": "251556368777322@lid",
        "sender_display_name": "Saved Contact",
        "from_name": "John Doe",
        "timestamp": "2023-10-15T10:30:00Z",
        "is_from_me": false,
        "body": "Hello, how are you?"
      }
    }"#;

    // GOWA docs/webhook-payload.md — image, auto-download, with caption
    const IMAGE: &str = r#"{
      "event": "message",
      "device_id": "628987654321@s.whatsapp.net",
      "payload": {
        "id": "3EB0C127D7BACC83D6A3",
        "chat_id": "628987654321@s.whatsapp.net",
        "from": "628123456789@s.whatsapp.net",
        "from_name": "John Doe",
        "timestamp": "2025-07-13T11:05:51Z",
        "body": "Check this out!",
        "image": {
          "path": "statics/media/1752404751-ad9e37ac-c658-4fe5-8d25-ba4a3f4d58fd.jpeg",
          "caption": "Check this out!"
        }
      }
    }"#;

    // GOWA docs/webhook-payload.md — audio
    const AUDIO: &str = r#"{
      "event": "message",
      "device_id": "628987654321@s.whatsapp.net",
      "payload": {
        "id": "3EB0C127D7BACC83D6A5",
        "chat_id": "628987654321@s.whatsapp.net",
        "from": "628123456789@s.whatsapp.net",
        "from_name": "John Doe",
        "timestamp": "2023-10-15T10:55:00Z",
        "audio": "statics/media/1752404905-b9393cd1-8546-4df9-8a60-ee3276036aba.ogg"
      }
    }"#;

    // GOWA docs/webhook-payload.md — delivery receipt
    const ACK: &str = r#"{
      "event": "message.ack",
      "device_id": "628123456789@s.whatsapp.net",
      "timestamp": "2025-07-18T22:44:20Z",
      "payload": {
        "ids": ["3EB00106E8BE0F407E88EC"],
        "chat_id": "120363402106XXXXX@g.us",
        "from": "6289685XXXXXX@s.whatsapp.net",
        "from_lid": "251556368777322@lid",
        "sender_display_name": "Saved Contact",
        "receipt_type": "delivered",
        "receipt_type_description": "means the message was delivered to the device (but the user might not have noticed)."
      }
    }"#;

    // GOWA: a group message mentioning the account. The docs print none; built from the fields
    // event_message.go always sets (chat_id @g.us, from = the member, "@<number>" in body).
    const GROUP_MENTION: &str = r#"{"event":"message","device_id":"628987654321@s.whatsapp.net","payload":{"id":"G1","chat_id":"120363402106000001@g.us","from":"628123456789@s.whatsapp.net","from_name":"John Doe","timestamp":"2025-07-13T11:05:51Z","is_from_me":false,"body":"@628987654321 tu viens ?"}}"#;

    fn message(body: &str) -> Message {
        match parse_webhook(body.as_bytes(), None) {
            Some(WebhookEvent::Message(m)) => m,
            other => panic!("not a message: {other:?}"),
        }
    }

    #[test]
    fn a_text_message_from_the_docs() {
        let m = message(TEXT);
        assert_eq!((m.id.as_str(), m.chat.as_str()), ("3EB0C127D7BACC83D6A1", "628987654321@s.whatsapp.net"));
        // The saved contact's name comes first.
        assert_eq!(m.sender_name, "Saved Contact");
        assert_eq!((m.text.as_str(), &m.media), ("Hello, how are you?", &Media::None));
        assert_eq!(m.date, 1_697_365_800);
        assert!(!m.from_me);
    }

    #[test]
    fn media_are_labels_only() {
        let image = message(IMAGE);
        assert_eq!((&image.media, image.text.as_str()), (&Media::Image, "Check this out!"));
        assert!(!image.from_me, "is_from_me missing means not mine");
        assert_eq!(message(AUDIO).media, Media::Voice { seconds: None });
    }

    #[test]
    fn receipts_and_garbage_are_not_messages() {
        assert_eq!(parse_webhook(ACK.as_bytes(), None), Some(WebhookEvent::Ignored));
        assert_eq!(parse_webhook(b"nope", None), None);
        assert_eq!(parse_webhook(b"[1]", None), None);
        assert_eq!(parse_webhook(br#"{"event":"message","payload":{}}"#, None), Some(WebhookEvent::Ignored));
    }

    #[test]
    fn a_group_mention_of_the_account_reaches_the_pill() {
        let m = message(GROUP_MENTION);
        assert!(m.mentions_me);
        assert_eq!(m.sender, "628123456789@s.whatsapp.net");
        let mut inbox = model::Inbox::default();
        assert_eq!(inbox.add(m, &model::Filters::default(), None, false), model::Outcome::Notify);
        // The same text in a private chat is no group mention.
        assert!(!message(&TEXT.replace("Hello, how are you?", "@628987654321")).mentions_me);
    }

    #[test]
    fn signatures_match_hmac_sha256() {
        // RFC 4231 test case 2 (GOWA prints no fixture; same HMAC-SHA256, hex lowercase).
        assert_eq!(
            signature(b"what do ya want for nothing?", "Jefe"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than a block.
        let long = "k".repeat(100);
        let sig = signature(b"x", &long);
        assert_eq!(sig.len(), 71);
        let headers = HashMap::from([(SIGNATURE_HEADER.to_string(), sig)]);
        assert!(verify(&headers, b"x", &long));
    }

    #[test]
    fn a_delivery_is_accepted_only_with_the_right_signature_over_the_exact_bytes() {
        let secret = "test-secret-0123456789";
        let body = r#"{"event":"message","payload":{"body":"héllo 👋"}}"#.as_bytes();
        let header = signature(body, secret);
        let with = |h: &str| HashMap::from([(SIGNATURE_HEADER.to_string(), h.to_string())]);
        assert!(verify(&with(&header), body, secret));
        let mut flipped = body.to_vec();
        flipped[3] ^= 1;
        assert!(!verify(&with(&header), &flipped, secret), "a flipped byte fails");
        assert!(!verify(&with(&header), body, &format!("{secret}x")), "the wrong secret fails");
        assert!(!verify(&with(&header[7..]), body, secret), "no prefix fails");
        assert!(!verify(&with(&header.replace("sha256=", "SHA256=")), body, secret), "SHA256= fails");
        assert!(!verify(&with(&header[..header.len() - 1]), body, secret), "truncated fails");
        assert!(!verify(&HashMap::new(), body, secret), "a missing header fails");
        assert!(!verify(&with(&header), body, ""), "an empty secret fails");
        assert!(!verify(&HashMap::from([("x-openwa-signature".to_string(), header.clone())]), body, secret));
    }

    #[test]
    fn the_device_coucou_works_on() {
        let devices = br#"{"code":"SUCCESS","message":"List devices","results":[{"id":"coucou","state":"disconnected","created_at":"2024-01-01T00:00:00Z"},{"id":"my-device-id","phone_number":"628123456789","display_name":"John Doe","state":"logged_in","jid":"628123456789@s.whatsapp.net","created_at":"2024-01-01T00:00:00Z"}]}"#;
        let (id, status) = device(devices).unwrap();
        assert_eq!(id, "my-device-id");
        assert_eq!(status, Status { link: Link::Connected, me: Some("628123456789@s.whatsapp.net".into()) });
        let fresh = br#"{"code":"SUCCESS","message":"List devices","results":[{"id":"coucou","state":"disconnected","created_at":"2024-01-01T00:00:00Z"}]}"#;
        assert_eq!(device(fresh).unwrap().1.link, Link::NeedsQr, "never logged in: a QR");
        assert!(device(br#"{"code":"SUCCESS","message":"x","results":[]}"#).is_none());
        assert!(device(br#"{"code":"SUCCESS","message":"x","results":null}"#).is_none());
    }

    #[test]
    fn the_qr_is_fetched_from_the_gateway_itself() {
        let login = br#"{"code":"SUCCESS","message":"Success","results":{"device_id":"coucou","qr_duration":30,"qr_link":"http://localhost:3000/statics/qrcode/scan-qr-b0b7bb43-9a22-455a-814f-5a225c743310.png"}}"#;
        assert_eq!(
            qr_path(login),
            Some(("/statics/qrcode/scan-qr-b0b7bb43-9a22-455a-814f-5a225c743310.png".to_string(), 30))
        );
        // Only the path is kept: a link to another host leads nowhere else.
        let evil = br#"{"code":"SUCCESS","message":"x","results":{"qr_link":"https://evil.example/statics/qr.png"}}"#;
        assert_eq!(qr_path(evil), Some(("/statics/qr.png".to_string(), 30)));
        assert_eq!(qr_path(br#"{"results":{}}"#), None);
        assert_eq!(error_code(br#"{"code":"ALREADY_LOGGED_IN","message":"you are already logged in."}"#).as_deref(), Some("ALREADY_LOGGED_IN"));
    }

    #[test]
    fn gowa_is_started_on_this_pc_only_without_downloading_media() {
        assert_eq!(launch_args(3001), ["rest", "--host=127.0.0.1", "--port=3001", "--auto-download-media=false"]);
    }

    #[test]
    fn stored_messages_and_chats() {
        let msgs = br#"{"code":"SUCCESS","message":"ok","results":{"data":[{"id":"M2","chat_jid":"120363402106000001@g.us","sender_jid":"628123456789@s.whatsapp.net","sender_display_name":"John","content":"","timestamp":"2025-07-13T11:06:00Z","is_from_me":false,"media_type":"document","filename":"devis.pdf"},{"id":"M1","chat_jid":"120363402106000001@g.us","sender_jid":"628987654321@s.whatsapp.net","content":"salut","timestamp":"2025-07-13T11:05:00Z","is_from_me":true,"media_type":""}],"pagination":{"limit":10,"offset":0,"total":2},"chat_info":{"jid":"120363402106000001@g.us","name":"Famille","last_message_time":"2025-07-13T11:06:00Z"}}}"#;
        let list = messages(msgs, None);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].media, Media::Document { name: Some("devis.pdf".into()) });
        assert!(list[1].from_me);
        assert_eq!(list[0].chat_name.as_deref(), Some("Famille"));
        let chats_json = br#"{"code":"SUCCESS","message":"ok","results":{"data":[{"jid":"628123456789@s.whatsapp.net","name":"John","last_message_time":"2025-07-13T11:06:00Z","archived":false}],"pagination":{"limit":25,"offset":0,"total":1}}}"#;
        let c = chats(chats_json);
        assert_eq!((c[0].name.as_str(), c[0].last), ("John", 1_752_404_760));
        assert_eq!(sent_id(br#"{"code":"SUCCESS","message":"ok","results":{"message_id":"3EB0AB","status":"sent"}}"#).as_deref(), Some("3EB0AB"));
        assert_eq!(group_name(br#"{"code":"SUCCESS","results":{"Name":"Famille"}}"#).as_deref(), Some("Famille"));
    }

    fn request(head: &str, body: &str) -> Vec<u8> {
        format!("{head}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn a_webhook_request_is_read_whole_or_not_at_all() {
        let raw = request("POST /whatsapp HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Hub-Signature-256: sha256=ab", "{\"a\":1}");
        match parse_request(&raw) {
            Parse::Done(r) => {
                assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/whatsapp"));
                assert_eq!(r.headers.get("x-hub-signature-256").map(String::as_str), Some("sha256=ab"));
                assert_eq!(r.body, b"{\"a\":1}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_request(&raw[..raw.len() - 2]), Parse::Incomplete);
        assert_eq!(parse_request(b"POST /whatsapp HTTP/1.1\r\nHost"), Parse::Incomplete);
        assert_eq!(parse_request(b"\r\n\r\n"), Parse::Invalid);
        assert_eq!(parse_request(b"POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"), Parse::Invalid);
        let long = vec![b'a'; MAX_HEAD + 1];
        assert_eq!(parse_request(&long), Parse::Invalid);
        assert_eq!(response(200, "OK"), b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
}
