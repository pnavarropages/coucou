// WhatsApp in the island (experimental): your incoming WhatsApp messages in a green pill, one row
// per conversation, and a reply from the island. Port of the Mac's 0.5.1 integration
// (WhatsAppController, WhatsAppGOWAInstaller, WhatsAppGOWAProcess…); docs/INTEGRATIONS.md §6bis.
//
// There is no WhatsApp API for a personal account, so Coucou talks to an unofficial gateway on this
// PC, GOWA (github.com/aldinokemal/go-whatsapp-web-multidevice), which Coucou downloads from GOWA's
// GitHub releases when the user clicks "Install WhatsApp", starts on 127.0.0.1, and stops when
// Coucou quits. Off unless the WhatsApp pill is among the active ones.
//
// Privacy: nothing but the local gateway is talked to (the one other call is the installer's, to
// GitHub, after a click). Messages stay in memory, are never logged or written to disk, and are
// forgotten when Coucou quits or the pill is turned off. No photo, no media is ever downloaded.
// Nothing is sent without the user's own action: a reply, in a conversation that wrote first, a
// few a minute at most. GOWA's credential and the webhook's secret live in the Credential Manager.

mod client;
mod core;
mod gateway;
mod installer;
mod model;
mod prefs;
mod process;
mod release;
mod webhook;

#[cfg(test)]
mod testutil;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use self::core::{Core, Host, PrefsChange, Secrets, Snapshot, Status, PILL_ID};
use crate::integrations::{IntegrationEvent, IntegrationUpdate};
use crate::{island, secrets};

/// The island hears of every change; a new message lights its pill.
struct TauriHost {
    app: AppHandle,
}

impl Host for TauriHost {
    fn snapshot(&self, snapshot: Snapshot) {
        let _ = self.app.emit_to(island::WINDOW_LABEL, "whatsapp-state", snapshot);
    }

    fn announce(&self, chat_name: &str) {
        // The island treats it as any integration's news: the pill finishes and gets its badge, a
        // sound plays, the compact island shows. Only the conversation's name goes there, never the text.
        let update = IntegrationUpdate {
            id: PILL_ID,
            data: json!({}),
            error: None,
            event: Some(IntegrationEvent { success: true, label: chat_name.to_string(), detail: None }),
        };
        let _ = self.app.emit_to(island::WINDOW_LABEL, "integration", update);
    }
}

/// The OS keystore (secrets.rs), for entries only Rust reads.
struct Keystore;

impl Secrets for Keystore {
    fn get(&self, key: &str) -> Option<String> {
        secrets::get_internal(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        secrets::set_internal(key, value)
    }
}

pub struct WhatsApp {
    core: Arc<Core>,
}

/// Called once, before the windows exist.
pub fn setup(app: &AppHandle) {
    let host = Arc::new(TauriHost { app: app.clone() });
    app.manage(WhatsApp { core: Core::new(host, Arc::new(Keystore), prefs::load()) });
}

/// Follows the active pills: on while the WhatsApp pill is among them. Called at launch and
/// whenever Settings are saved.
pub fn sync(app: &AppHandle, active_integrations: &[String]) {
    if let Some(whatsapp) = app.try_state::<WhatsApp>() {
        whatsapp.core.set_should_run(active_integrations.iter().any(|id| id == PILL_ID));
    }
}

/// Coucou is quitting: GOWA goes with it.
pub fn stop_on_exit(app: &AppHandle) {
    if let Some(whatsapp) = app.try_state::<WhatsApp>() {
        whatsapp.core.shutdown();
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// The windows a command may be called from: the island shows and answers conversations, Settings
/// installs, links and filters. Nothing else may touch WhatsApp.
fn only(window: &WebviewWindow, labels: &[&str]) -> Result<(), String> {
    if labels.contains(&window.label()) {
        Ok(())
    } else {
        Err("this window may not use WhatsApp".into())
    }
}

const ISLAND: &str = island::WINDOW_LABEL;
const SETTINGS: &str = "settings";

/// The conversations the pill lists. The island asks once at start; changes come as events.
#[tauri::command]
pub fn whatsapp_snapshot(window: WebviewWindow, state: State<WhatsApp>) -> Result<Snapshot, String> {
    only(&window, &[ISLAND])?;
    Ok(state.core.snapshot())
}

/// The link, the QR to scan, what is installed. Never a message.
#[tauri::command]
pub fn whatsapp_status(window: WebviewWindow, state: State<WhatsApp>) -> Result<Status, String> {
    only(&window, &[SETTINGS])?;
    Ok(state.core.status())
}

#[tauri::command]
pub fn whatsapp_install(window: WebviewWindow, state: State<WhatsApp>) -> Result<(), String> {
    only(&window, &[SETTINGS])?;
    state.core.install();
    Ok(())
}

#[tauri::command]
pub fn whatsapp_set_prefs(window: WebviewWindow, state: State<WhatsApp>, change: PrefsChange) -> Result<(), String> {
    only(&window, &[SETTINGS])?;
    state.core.set_prefs(change)
}

#[tauri::command]
pub fn whatsapp_open(window: WebviewWindow, state: State<WhatsApp>, chat: String) -> Result<(), String> {
    only(&window, &[ISLAND])?;
    state.core.open(&chat);
    Ok(())
}

#[tauri::command]
pub fn whatsapp_close(window: WebviewWindow, state: State<WhatsApp>, chat: String) -> Result<(), String> {
    only(&window, &[ISLAND])?;
    state.core.close(&chat);
    Ok(())
}

/// A reply, typed and sent by the user. Err is the sentence for the field (empty: nothing to say).
#[tauri::command]
pub async fn whatsapp_send(window: WebviewWindow, state: State<'_, WhatsApp>, chat: String, text: String) -> Result<(), String> {
    only(&window, &[ISLAND])?;
    state.core.send(&chat, &text).await
}

#[tauri::command]
pub fn whatsapp_ignore(window: WebviewWindow, state: State<WhatsApp>, chat: String) -> Result<(), String> {
    only(&window, &[ISLAND, SETTINGS])?;
    state.core.ignore(&chat);
    Ok(())
}

#[tauri::command]
pub fn whatsapp_unignore(window: WebviewWindow, state: State<WhatsApp>, chat: String) -> Result<(), String> {
    only(&window, &[SETTINGS])?;
    state.core.unignore(&chat);
    Ok(())
}
