// HTTP to the local GOWA: `./whatsapp rest` (port 3011 by default). Each call goes to the address
// set in Settings (127.0.0.1) and nowhere else: no proxy, no redirect, no cookie, no photo, no
// media, nothing from WhatsApp's own servers. Auth: HTTP basic `user:pass`, the credential
// Coucou generated when it started GOWA. Coucou works on one device (the one already logged
// in, else a new one named "coucou"). Endpoints, headers and bodies follow GOWA's docs (see
// gateway.rs for the references); port of GOWAProvider in WhatsAppProvider.swift.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::{Method, Url};
use serde_json::{json, Value};

use super::gateway::{self, ChatSummary, Link, Status};
use super::model::{self, Message};
use crate::net;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaError {
    /// Nothing answers at that address.
    Unreachable,
    /// Wrong user:pass.
    Unauthorized,
    /// The gateway answered with an error (its own code, never a message's content).
    Refused(String),
    /// WhatsApp isn't linked yet.
    NotLoggedIn,
}

const TIMEOUT: Duration = Duration::from_secs(10);
const QR_TIMEOUT: Duration = Duration::from_secs(45);

pub struct Gowa {
    base: Url,
    auth: Option<(String, String)>,
    http: reqwest::Client,
    device: Mutex<Option<String>>,
}

/// A path segment, percent-encoded where needed ("@" and the usual id characters stay).
pub fn path_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Gowa {
    /// `credential` is "user:password".
    pub fn new(base: Url, credential: Option<&str>) -> Result<Gowa, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // Straight to the local gateway, never through a proxy; and no redirect: a request
            // (with its credentials and, for a send, the reply's text) goes nowhere else.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let auth = credential
            .and_then(|c| c.split_once(':'))
            .map(|(user, pass)| (user.to_string(), pass.to_string()));
        Ok(Gowa { base, auth, http, device: Mutex::new(None) })
    }

    /// `path` is appended to the base address as given (already percent-encoded where needed).
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, u16), WaError> {
        let mut url = self.base.clone();
        let prefix = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{prefix}{path}"));
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let mut request = self.http.request(method, url.clone()).timeout(timeout).header("Accept", "application/json");
        if let Some((user, pass)) = &self.auth {
            request = request.basic_auth(user, Some(pass));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| WaError::Unreachable)?;
        // Second lock: an answer from anywhere but the gateway is not one.
        if response.url().host_str() != url.host_str() || response.url().port() != url.port() {
            return Err(WaError::Unreachable);
        }
        let code = response.status().as_u16();
        let data = net::read_capped(response, net::MAX_BODY).await.map_err(|_| WaError::Unreachable)?;
        Ok((data, code))
    }

    fn check(data: &[u8], code: u16) -> Result<(), WaError> {
        if code == 401 {
            // GOWA's AUTHENTICATION_ERROR says the device isn't logged in; any other 401 is the
            // basic auth being refused.
            return Err(if gateway::error_code(data).as_deref() == Some("AUTHENTICATION_ERROR") {
                WaError::NotLoggedIn
            } else {
                WaError::Unauthorized
            });
        }
        if !(200..300).contains(&code) {
            return Err(WaError::Refused(gateway::error_code(data).unwrap_or_else(|| format!("HTTP {code}"))));
        }
        Ok(())
    }

    fn cached_device(&self) -> Option<String> {
        self.device.lock().unwrap().clone()
    }

    /// The device to work on; created ("coucou") when the gateway has none.
    async fn device(&self) -> Result<(String, Status), WaError> {
        let (data, code) = self.call(Method::GET, "/devices", &[], None, TIMEOUT).await?;
        Self::check(&data, code)?;
        if let Some((id, status)) = gateway::device(&data) {
            *self.device.lock().unwrap() = Some(id.clone());
            return Ok((id, status));
        }
        let (made, code) = self.call(Method::POST, "/devices", &[], Some(json!({ "device_id": "coucou" })), TIMEOUT).await?;
        Self::check(&made, code)?;
        *self.device.lock().unwrap() = Some("coucou".into());
        Ok(("coucou".into(), Status { link: Link::NeedsQr, me: None }))
    }

    async fn device_id(&self) -> Result<String, WaError> {
        match self.cached_device() {
            Some(id) => Ok(id),
            None => Ok(self.device().await?.0),
        }
    }

    async fn scoped(&self, method: Method, path: &str, mut query: Vec<(&str, String)>, body: Option<Value>) -> Result<Vec<u8>, WaError> {
        let id = self.device_id().await?;
        query.push(("device_id", id));
        let (data, code) = self.call(method, path, &query, body, TIMEOUT).await?;
        Self::check(&data, code)?;
        Ok(data)
    }

    pub async fn status(&self) -> Result<Status, WaError> {
        Ok(self.device().await?.1)
    }

    /// The QR to scan, as PNG bytes, and how long it stays valid; None when there is none to show
    /// (already linked, not ready yet).
    pub async fn qr_code(&self) -> Result<Option<(Vec<u8>, u64)>, WaError> {
        let id = self.device_id().await?;
        // GOWA answers once WhatsApp has handed it the first code: a few seconds.
        let (data, code) = self.call(Method::GET, &format!("/devices/{}/login", path_component(&id)), &[], None, QR_TIMEOUT).await?;
        if gateway::error_code(&data).as_deref() == Some("ALREADY_LOGGED_IN") {
            return Ok(None);
        }
        Self::check(&data, code)?;
        let Some((path, seconds)) = gateway::qr_path(&data) else { return Ok(None) };
        // The PNG is served by the gateway itself under /statics: fetched from the configured
        // address, whatever host the link names.
        let (png, png_code) = self.call(Method::GET, &path, &[], None, TIMEOUT).await?;
        if png_code != 200 || !png.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            return Ok(None);
        }
        // qr_duration is half the code's life (the PNG is deleted then, the code stays valid):
        // asking again sooner would restart the pairing under the user's phone.
        Ok(Some((png, (seconds * 2).saturating_sub(5).max(20))))
    }

    /// Tells GOWA to post new messages to Coucou's local webhook, signed with `secret`.
    pub async fn register_webhook(&self, url: &str, secret: &str) -> Result<(), WaError> {
        let id = self.device_id().await?;
        let (data, code) = self
            .call(
                Method::PATCH,
                &format!("/devices/{}/webhook", path_component(&id)),
                &[],
                Some(json!({ "webhook_url": url, "webhook_secret": secret, "webhook_events": "message" })),
                TIMEOUT,
            )
            .await?;
        Self::check(&data, code)
    }

    /// Messages of the chats that moved since `since` (Unix seconds), oldest first.
    pub async fn recent_messages(&self, since: i64, me: Option<&str>) -> Result<Vec<Message>, WaError> {
        let chats: Vec<ChatSummary> = gateway::chats(&self.scoped(Method::GET, "/chats", vec![("limit", "20".into())], None).await?);
        let mut out = Vec::new();
        // Only the chats that moved since `since` (sorted newest first by the gateway).
        for chat in chats.iter().take(8).filter(|c| c.last > since) {
            let data = self
                .scoped(
                    Method::GET,
                    &format!("/chat/{}/messages", path_component(&chat.jid)),
                    vec![("limit", "10".into()), ("start_time", model::iso(since))],
                    None,
                )
                .await?;
            for mut message in gateway::messages(&data, me) {
                if message.chat_name.is_none() && !chat.name.is_empty() && model::is_group(&chat.jid) {
                    message.chat_name = Some(chat.name.clone());
                }
                out.push(message);
            }
        }
        out.sort_by_key(|m| m.date);
        Ok(out)
    }

    /// The latest `limit` messages of one chat, oldest first.
    pub async fn messages(&self, chat: &str, limit: usize, me: Option<&str>) -> Result<Vec<Message>, WaError> {
        let data = self
            .scoped(Method::GET, &format!("/chat/{}/messages", path_component(chat)), vec![("limit", limit.to_string())], None)
            .await?;
        let mut list = gateway::messages(&data, me);
        list.sort_by_key(|m| m.date);
        Ok(list)
    }

    /// Sends a text to an existing chat; returns the new message's id when GOWA gives it.
    pub async fn send(&self, text: &str, chat: &str) -> Result<Option<String>, WaError> {
        let data = self.scoped(Method::POST, "/send/message", vec![], Some(json!({ "phone": chat, "message": text }))).await?;
        Ok(gateway::sent_id(&data))
    }

    /// Marks the chat read up to `last_incoming` (the newest message received in it).
    pub async fn mark_read(&self, chat: &str, last_incoming: Option<&str>) -> Result<(), WaError> {
        let Some(id) = last_incoming else { return Ok(()) };
        self.scoped(Method::POST, &format!("/message/{}/read", path_component(id)), vec![], Some(json!({ "phone": chat }))).await?;
        Ok(())
    }

    /// A group's name, when the messages don't carry it.
    pub async fn group_name(&self, chat: &str) -> Option<String> {
        if !model::is_group(chat) {
            return None;
        }
        let data = self.scoped(Method::GET, "/group/info", vec![("group_id", chat.to_string())], None).await.ok()?;
        gateway::group_name(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(path_component("33612345678@s.whatsapp.net"), "33612345678@s.whatsapp.net");
        assert_eq!(path_component("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(path_component("3EB0-C1_27.~"), "3EB0-C1_27.~");
    }

    #[test]
    fn a_refused_login_is_not_a_missing_whatsapp_link() {
        let auth = br#"{"code":"AUTHENTICATION_ERROR","message":"not logged in"}"#;
        assert_eq!(Gowa::check(auth, 401), Err(WaError::NotLoggedIn));
        assert_eq!(Gowa::check(b"Unauthorized", 401), Err(WaError::Unauthorized));
        assert_eq!(Gowa::check(br#"{"code":"DEVICE_ID_REQUIRED"}"#, 400), Err(WaError::Refused("DEVICE_ID_REQUIRED".into())));
        assert_eq!(Gowa::check(b"", 500), Err(WaError::Refused("HTTP 500".into())));
        assert_eq!(Gowa::check(b"{}", 200), Ok(()));
    }
}
