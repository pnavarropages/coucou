// WhatsApp in the island. Talks to the GOWA gateway on this PC (started by Coucou), receives new
// messages on a webhook bound to 127.0.0.1, falls back to polling when no webhook comes, and
// keeps the conversations in memory only: nothing of a message is ever written to disk or to a
// log, and everything is forgotten when Coucou quits or the integration is turned off. Runs only
// while the WhatsApp pill is among the active ones. Port of WhatsAppController.swift.
//
// Nothing here knows Tauri: the app is reached through `Host` (island events) and `Secrets` (the
// Credential Manager), so the tests drive it against a simulated gateway.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::client::{Gowa, WaError};
use super::gateway::{self, Link as WaLink, WebhookEvent};
use super::installer::{self, Source, Step};
use super::model::{self, Conversation, Inbox, Message, Outcome, RateLimit, Refusal};
use super::prefs::Prefs;
use super::process::{GowaProcess, Launch};
use super::release::Target;
use super::webhook;
use crate::i18n::{t, tf};
use crate::log;

pub const PILL_ID: &str = "integration_whatsapp";
pub const WEBHOOK_PATH: &str = "/whatsapp";

/// Keys of the Credential Manager (secrets::INTERNAL_KEYS).
pub const CREDENTIAL_KEY: &str = "whatsapp-gowa-auth";
pub const SECRET_KEY: &str = "whatsapp-webhook-secret";

/// What the app lends the controller.
pub trait Host: Send + Sync + 'static {
    /// The island's picture changed.
    fn snapshot(&self, snapshot: Snapshot);
    /// A new message reached you: the pill lights up with the conversation's name, never the text.
    fn announce(&self, chat_name: &str);
}

pub trait Secrets: Send + Sync + 'static {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Off,
    Connecting,
    /// The gateway runs, WhatsApp isn't linked: the QR is shown in Settings.
    NeedsQr,
    Connected,
    Unreachable,
    Unauthorized,
    Failed(String),
}

impl LinkState {
    fn code(&self) -> &'static str {
        match self {
            LinkState::Off => "off",
            LinkState::Connecting => "connecting",
            LinkState::NeedsQr => "needsQr",
            LinkState::Connected => "connected",
            LinkState::Unreachable => "unreachable",
            LinkState::Unauthorized => "unauthorized",
            LinkState::Failed(_) => "failed",
        }
    }

    fn message(&self) -> Option<String> {
        match self {
            LinkState::Failed(why) => Some(why.clone()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebhookState {
    Off,
    /// Listening on the port, not yet accepted by the gateway.
    Listening,
    /// The gateway accepted the address: messages should come on their own.
    Registered,
    /// The gateway refused it (its message): polling instead.
    Refused(String),
    /// The port is taken.
    PortBusy,
}

impl WebhookState {
    fn code(&self) -> &'static str {
        match self {
            WebhookState::Off => "off",
            WebhookState::Listening => "listening",
            WebhookState::Registered => "registered",
            WebhookState::Refused(_) => "refused",
            WebhookState::PortBusy => "portBusy",
        }
    }
}

/// What the island shows: the conversations the pill lists, and the state of a reply.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub link: &'static str,
    pub link_message: Option<String>,
    pub me: Option<String>,
    pub conversations: Vec<Conversation>,
    pub unread: u32,
    /// The gateway has been down twice in a row (not one still starting): a small red badge on the pill.
    pub down: bool,
    pub sending: bool,
    pub send_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IgnoredChat {
    pub id: String,
    pub name: String,
}

/// What Settings shows. Never a message.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub link: &'static str,
    pub link_message: Option<String>,
    pub me: Option<String>,
    pub webhook: &'static str,
    /// What the webhook's state means, in a sentence.
    pub webhook_line: String,
    /// The QR to scan, as a data: URL.
    pub qr: Option<String>,
    pub installed: bool,
    pub version: Option<String>,
    pub install: Step,
    pub supported: bool,
    /// Where GOWA and its WhatsApp session live.
    pub folder: String,
    pub url: String,
    pub webhook_port: u16,
    pub auto_start: bool,
    pub mark_read_on_open: bool,
    pub mute_groups: bool,
    pub ignored: Vec<IgnoredChat>,
}

/// The settings the window can change in one go.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefsChange {
    pub url: String,
    pub webhook_port: u16,
    pub auto_start: bool,
    pub mark_read_on_open: bool,
    pub mute_groups: bool,
}

struct State {
    prefs: Prefs,
    should_run: bool,
    running: bool,
    generation: u64,
    client: Option<Arc<Gowa>>,
    link: LinkState,
    webhook: WebhookState,
    qr: Option<Vec<u8>>,
    qr_expiry: Instant,
    me: Option<String>,
    inbox: Inbox,
    install: Step,
    sending: bool,
    send_error: Option<String>,
    /// The conversation open in the card: what arrives there is seen, so it isn't unread.
    open_chat: Option<String>,
    rate: RateLimit,
    /// Failed checks in a row (the gateway down, refusing, unreachable).
    failures: u32,
    tick: u64,
    refreshing: bool,
    polling: bool,
    backfilled: bool,
    last_poll: i64,
    last_webhook: Option<Instant>,
    last_announce: Option<Instant>,
    last_tick: Option<Instant>,
    naming: HashSet<String>,
    loop_task: Option<tauri::async_runtime::JoinHandle<()>>,
    listener_task: Option<tauri::async_runtime::JoinHandle<()>>,
}

pub struct Core {
    host: Arc<dyn Host>,
    secrets: Arc<dyn Secrets>,
    state: Mutex<State>,
    process: GowaProcess,
    /// Whether Coucou starts GOWA itself (the tests have a simulated one).
    manage_process: bool,
    /// Where GOWA's binary is (and its session).
    dir: PathBuf,
    source: Source,
    /// Where the preferences are saved; None in the tests.
    save_to: Option<PathBuf>,
    /// The tests drive the checks by hand.
    spawn_loop: bool,
    stopping: AtomicBool,
}

/// A check every five seconds until linked, then every minute, with the messages every twenty
/// seconds unless the webhook delivers.
const TICK: Duration = Duration::from_secs(5);
/// Webhooks that keep coming: no need to poll.
const WEBHOOK_ALIVE: Duration = Duration::from_secs(300);

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn describe(error: &WaError) -> String {
    match error {
        WaError::Unreachable => t("The gateway doesn’t answer: is it running?"),
        WaError::Unauthorized => t("The gateway refuses the credentials"),
        WaError::NotLoggedIn => t("Scan the QR code in Settings"),
        WaError::Refused(why) => why.clone(),
    }
}

impl Core {
    pub fn new(host: Arc<dyn Host>, secrets: Arc<dyn Secrets>, prefs: Prefs) -> Arc<Core> {
        Self::with(host, secrets, prefs, true, installer::dir(), Source::github(), Some(crate::settings::config_dir().join("whatsapp.json")), true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with(
        host: Arc<dyn Host>,
        secrets: Arc<dyn Secrets>,
        prefs: Prefs,
        manage_process: bool,
        dir: PathBuf,
        source: Source,
        save_to: Option<PathBuf>,
        spawn_loop: bool,
    ) -> Arc<Core> {
        Arc::new(Core {
            host,
            secrets,
            state: Mutex::new(State {
                prefs,
                should_run: false,
                running: false,
                generation: 0,
                client: None,
                link: LinkState::Off,
                webhook: WebhookState::Off,
                qr: None,
                qr_expiry: Instant::now(),
                me: None,
                inbox: Inbox::default(),
                install: Step::Idle,
                sending: false,
                send_error: None,
                open_chat: None,
                rate: RateLimit::new(6, Duration::from_secs(60)),
                failures: 0,
                tick: 0,
                refreshing: false,
                polling: false,
                backfilled: false,
                last_poll: model::now_secs(),
                last_webhook: None,
                last_announce: None,
                last_tick: None,
                naming: HashSet::new(),
                loop_task: None,
                listener_task: None,
            }),
            process: GowaProcess::default(),
            manage_process,
            dir,
            source,
            save_to,
            spawn_loop,
            stopping: AtomicBool::new(false),
        })
    }

    // ── Secrets ──────────────────────────────────────────────────────────────

    /// GOWA's basic auth ("user:password"), made once and kept in the Credential Manager.
    fn credential(&self) -> String {
        if let Some(c) = self.secrets.get(CREDENTIAL_KEY).filter(|c| c.contains(':') && c.len() >= 16) {
            return c;
        }
        let made = format!("coucou:{}", random_hex(16));
        if let Err(error) = self.secrets.set(CREDENTIAL_KEY, &made) {
            log::line(format!("WhatsApp: could not store GOWA's credential: {error}"));
        }
        made
    }

    /// The webhook's signing secret, made once (random, 64 hex characters).
    fn webhook_secret(&self) -> String {
        if let Some(s) = self.secrets.get(SECRET_KEY).filter(|s| s.len() >= 16) {
            return s;
        }
        let made = random_hex(32);
        if let Err(error) = self.secrets.set(SECRET_KEY, &made) {
            log::line(format!("WhatsApp: could not store the webhook secret: {error}"));
        }
        made
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// The pill was turned on or off (or Coucou just started): follow it.
    pub fn set_should_run(self: &Arc<Self>, on: bool) {
        {
            let mut st = lock(&self.state);
            if st.should_run == on {
                return;
            }
            st.should_run = on;
        }
        self.update();
    }

    /// Settings changed (gateway, port, installed): start over.
    pub fn settings_changed(self: &Arc<Self>) {
        let keep = lock(&self.state).should_run;
        self.stop(!keep);
        self.update();
    }

    fn update(self: &Arc<Self>) {
        let (want, running) = {
            let st = lock(&self.state);
            (st.should_run, st.running)
        };
        if want && !running {
            self.begin();
        } else if !want && running {
            self.stop(true);
        }
    }

    fn begin(self: &Arc<Self>) {
        let prefs = lock(&self.state).prefs.clone();
        let url = match prefs.gateway_url() {
            Ok(url) => url,
            Err(_) => {
                lock(&self.state).link = LinkState::Failed(t("The gateway URL isn’t valid (http://…)"));
                self.publish();
                return;
            }
        };
        let credential = self.credential();
        let client = match Gowa::new(url, Some(&credential)) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                lock(&self.state).link = LinkState::Failed(error);
                self.publish();
                return;
            }
        };
        let generation = {
            let mut st = lock(&self.state);
            st.generation += 1;
            st.running = true;
            st.client = Some(client);
            st.link = LinkState::Connecting;
            st.failures = 0;
            st.tick = 0;
            st.last_poll = model::now_secs();
            st.generation
        };
        // GOWA started by Coucou, before the first check.
        self.sync_process();
        if self.spawn_loop {
            let core = self.clone();
            let task = tauri::async_runtime::spawn(async move { core.run(generation).await });
            lock(&self.state).loop_task = Some(task);
        }
        self.publish();
    }

    /// Stops talking to the gateway. `forget`: the conversations go too.
    fn stop(self: &Arc<Self>, forget: bool) {
        let keep_running = {
            let mut st = lock(&self.state);
            st.generation += 1;
            if let Some(task) = st.loop_task.take() {
                task.abort();
            }
            // The webhook's port is let go with its task.
            if let Some(task) = st.listener_task.take() {
                task.abort();
            }
            st.running = false;
            st.refreshing = false;
            st.polling = false;
            st.client = None;
            st.link = LinkState::Off;
            st.webhook = WebhookState::Off;
            st.qr = None;
            st.me = None;
            st.backfilled = false;
            st.last_webhook = None;
            st.naming.clear();
            st.failures = 0;
            st.sending = false;
            if forget {
                st.open_chat = None;
                st.inbox.clear();
            }
            st.should_run
        };
        // Turned off: GOWA goes too. Otherwise begin() keeps it, or restarts it if its settings changed.
        if !keep_running {
            self.process.stop_now();
        }
        self.publish();
    }

    /// Coucou is quitting.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.process.stop_now();
    }

    /// Brings the GOWA process in line with the settings.
    fn sync_process(&self) {
        if !self.manage_process {
            return;
        }
        let prefs = lock(&self.state).prefs.clone();
        let binary = self.dir.join(installer::BINARY_NAME);
        let want = (prefs.auto_start && binary.is_file() && prefs.gateway_url().is_ok())
            .then(|| Launch::gowa(binary, prefs.gateway_port(), self.credential()));
        self.process.sync(want.as_ref());
    }

    /// The loop: a bound webhook, then a check every few seconds until the generation moves on.
    async fn run(self: Arc<Self>, generation: u64) {
        self.start_listener(generation).await;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if lock(&self.state).generation != generation {
                return;
            }
            self.on_tick().await;
        }
    }

    async fn on_tick(self: &Arc<Self>) {
        // Pausing Coucou means pausing it: nothing talks to the gateway, and GOWA itself rests (it
        // is started again by the first check after the pause).
        if crate::integrations::PAUSED.load(Ordering::Relaxed) {
            self.process.stop_now();
            return;
        }
        let (connected, failures, tick, slept) = {
            let mut st = lock(&self.state);
            st.tick += 1;
            // Back from sleep: nothing is trusted after it, ask the gateway again at once.
            let slept = st.last_tick.is_some_and(|t| t.elapsed() > Duration::from_secs(30));
            st.last_tick = Some(Instant::now());
            if slept {
                if st.webhook != WebhookState::PortBusy && st.webhook != WebhookState::Off {
                    st.webhook = WebhookState::Listening;
                }
                st.last_webhook = None;
            }
            (st.link == LinkState::Connected, st.failures, st.tick, slept)
        };
        if slept {
            self.refresh().await;
            return;
        }
        if !connected {
            // Every 5 s while linking (QR, gateway coming up); every 30 s once the gateway has been
            // down for a minute.
            if failures > 12 && tick % 6 != 0 {
                return;
            }
            self.refresh().await;
        } else {
            if tick % 12 == 0 {
                self.refresh().await; // every minute: still linked?
            }
            if tick % 4 == 0 {
                self.poll().await; // every 20 s, unless the webhook delivers
            }
        }
    }

    // ── Status and QR ────────────────────────────────────────────────────────

    pub async fn refresh(self: &Arc<Self>) {
        let (client, generation) = {
            let mut st = lock(&self.state);
            let Some(client) = st.client.clone() else { return };
            if st.refreshing {
                return;
            }
            st.refreshing = true;
            (client, st.generation)
        };
        self.check(&client, generation).await;
        let mut st = lock(&self.state);
        if st.generation == generation {
            st.refreshing = false;
        }
        drop(st);
        self.publish();
    }

    async fn check(self: &Arc<Self>, client: &Arc<Gowa>, generation: u64) {
        let status = match client.status().await {
            Ok(status) => status,
            Err(error) => return self.apply(error, generation),
        };
        let (register, backfill, ask_qr) = {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            st.failures = 0;
            st.me = status.me.clone();
            match status.link {
                WaLink::Connected => {
                    if st.link != LinkState::Connected {
                        st.qr = None;
                    }
                    st.link = LinkState::Connected;
                    (st.webhook == WebhookState::Listening, !st.backfilled, false)
                }
                WaLink::NeedsQr => {
                    st.link = LinkState::NeedsQr;
                    let ask = st.qr.is_none() || Instant::now() > st.qr_expiry;
                    if ask {
                        // Not asked again for 15 s even when nothing came: each GOWA login call
                        // restarts the pairing.
                        st.qr_expiry = Instant::now() + Duration::from_secs(15);
                    }
                    (false, false, ask)
                }
                WaLink::Connecting => {
                    st.link = LinkState::Connecting;
                    (false, false, false)
                }
            }
        };
        self.publish();
        if register {
            self.register(client, generation).await;
        }
        if backfill {
            self.backfill(client, generation).await;
        }
        if ask_qr {
            match client.qr_code().await {
                Ok(Some((png, seconds))) => {
                    let mut st = lock(&self.state);
                    if st.generation == generation {
                        st.qr = Some(png);
                        st.qr_expiry = Instant::now() + Duration::from_secs(seconds);
                    }
                }
                Ok(None) => {}
                Err(error) => self.apply(error, generation),
            }
        }
    }

    fn apply(self: &Arc<Self>, error: WaError, generation: u64) {
        let unreachable = error == WaError::Unreachable;
        {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            st.link = match &error {
                WaError::Unreachable => LinkState::Unreachable,
                WaError::Unauthorized => LinkState::Unauthorized,
                WaError::NotLoggedIn => LinkState::NeedsQr,
                WaError::Refused(why) => LinkState::Failed(why.clone()),
            };
            if error != WaError::NotLoggedIn {
                st.failures += 1;
            }
        }
        // GOWA started by Coucou and gone (crash, killed): started again, once a minute at most.
        if unreachable {
            self.sync_process();
        }
    }

    // ── Webhook ──────────────────────────────────────────────────────────────

    /// Binds the webhook's port and serves it for as long as this task lives.
    pub async fn start_listener(self: &Arc<Self>, generation: u64) {
        let port = lock(&self.state).prefs.webhook_port;
        let listener = match webhook::bind(port).await {
            Ok(listener) => listener,
            Err(_) => {
                let mut st = lock(&self.state);
                if st.generation == generation {
                    st.webhook = WebhookState::PortBusy;
                }
                drop(st);
                self.publish();
                return;
            }
        };
        {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            if st.webhook == WebhookState::Off || st.webhook == WebhookState::PortBusy {
                st.webhook = WebhookState::Listening;
            }
        }
        let secret = self.webhook_secret();
        let core = self.clone();
        let on_request: webhook::OnRequest = Arc::new(move |request| {
            if request.method != "POST" || !request.path.starts_with(WEBHOOK_PATH) {
                return 404;
            }
            // Signed by the gateway with Coucou's secret, over the exact bytes received.
            if !gateway::verify(&request.headers, &request.body, &secret) {
                return 401;
            }
            core.received(&request.body);
            200
        });
        // Serving runs in a task of its own: it ends, and the port is let go, when it is aborted.
        let task = tauri::async_runtime::spawn(webhook::serve(listener, on_request));
        {
            let mut st = lock(&self.state);
            if st.generation == generation {
                if let Some(old) = st.listener_task.replace(task) {
                    old.abort();
                }
            } else {
                task.abort();
            }
        }
        self.publish();
        // Registered with the gateway on the first check.
        self.refresh().await;
    }

    pub fn webhook_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", lock(&self.state).prefs.webhook_port, WEBHOOK_PATH)
    }

    async fn register(self: &Arc<Self>, client: &Arc<Gowa>, generation: u64) {
        let url = self.webhook_url();
        let result = client.register_webhook(&url, &self.webhook_secret()).await;
        let mut st = lock(&self.state);
        if st.generation != generation {
            return;
        }
        match result {
            Ok(()) => st.webhook = WebhookState::Registered,
            Err(WaError::Refused(why)) => {
                st.webhook = WebhookState::Refused(why);
                drop(st);
                log::line("WhatsApp: the gateway refused the webhook");
            }
            // Tried again on the next minute's check.
            Err(_) => {}
        }
    }

    /// One signed delivery from the gateway.
    pub fn received(self: &Arc<Self>, body: &[u8]) {
        let me = {
            let mut st = lock(&self.state);
            if st.client.is_none() {
                return;
            }
            st.last_webhook = Some(Instant::now());
            if st.webhook != WebhookState::Registered {
                st.webhook = WebhookState::Registered;
            }
            st.me.clone()
        };
        if let Some(WebhookEvent::Message(message)) = gateway::parse_webhook(body, me.as_deref()) {
            self.ingest(vec![message]);
        }
    }

    // ── Messages ─────────────────────────────────────────────────────────────

    /// The last hours of history, once linked: kept, but nothing counts as new.
    async fn backfill(self: &Arc<Self>, client: &Arc<Gowa>, generation: u64) {
        let (since, me) = {
            let mut st = lock(&self.state);
            st.backfilled = true;
            (model::now_secs() - 12 * 3600, st.me.clone())
        };
        let Ok(messages) = client.recent_messages(since, me.as_deref()).await else { return };
        {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            let filters = st.prefs.filters();
            // History: kept, never counted as unread (whether it was read is unknown), never announced.
            for message in &messages {
                st.inbox.add(message.clone(), &filters, None, true);
            }
            st.last_poll = model::now_secs();
        }
        for message in &messages {
            self.name_group_if_needed(&model::normalized(&message.chat));
        }
        self.publish();
    }

    pub async fn poll(self: &Arc<Self>) {
        let (client, generation, since, me, started) = {
            let mut st = lock(&self.state);
            let Some(client) = st.client.clone() else { return };
            let alive = st.last_webhook.is_some_and(|t| t.elapsed() < WEBHOOK_ALIVE);
            if st.polling || alive || !st.backfilled {
                return;
            }
            st.polling = true;
            // A little overlap; duplicates are dropped.
            (client, st.generation, st.last_poll - 30, st.me.clone(), model::now_secs())
        };
        let result = client.recent_messages(since, me.as_deref()).await;
        {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return;
            }
            st.polling = false;
            if result.is_ok() {
                st.last_poll = started;
            }
        }
        if let Ok(messages) = result {
            self.ingest(messages);
        }
    }

    fn ingest(self: &Arc<Self>, messages: Vec<Message>) {
        let mut fresh: Option<String> = None;
        let mut to_name = Vec::new();
        {
            let mut st = lock(&self.state);
            let filters = st.prefs.filters();
            for message in messages {
                let key = model::normalized(&message.chat);
                let from_me = message.from_me;
                let outcome = st.inbox.add(message, &filters, None, false);
                to_name.push(key.clone());
                if st.open_chat.as_deref() == Some(key.as_str()) {
                    // Read on screen: no unread. The receipt waits for a reply (or the next
                    // opening), as Settings says.
                    st.inbox.mark_read(&key);
                    continue;
                }
                if outcome == Outcome::Notify && !from_me {
                    fresh = Some(key);
                }
            }
        }
        for key in to_name {
            self.name_group_if_needed(&key);
        }
        self.publish();
        if let Some(key) = fresh {
            self.announce(&key);
        }
    }

    /// Groups arrive under their id (GOWA's webhook carries no group name): asked once.
    fn name_group_if_needed(self: &Arc<Self>, chat: &str) {
        if !model::is_group(chat) {
            return;
        }
        let (client, generation) = {
            let mut st = lock(&self.state);
            let Some(client) = st.client.clone() else { return };
            let unnamed = st.inbox.conversation(chat).is_some_and(|c| c.name == model::display_number(chat));
            if !unnamed || !st.naming.insert(chat.to_string()) {
                return;
            }
            (client, st.generation)
        };
        let core = self.clone();
        let chat = chat.to_string();
        tauri::async_runtime::spawn(async move {
            if let Some(name) = client.group_name(&chat).await {
                let mut st = lock(&core.state);
                if st.generation == generation {
                    st.inbox.rename(&chat, &name);
                    drop(st);
                    core.publish();
                }
            }
        });
    }

    /// A new message reaches you: the pill lights up with the conversation's name. A burst of
    /// messages is one reaction, not ten.
    fn announce(&self, chat: &str) {
        let name = {
            let mut st = lock(&self.state);
            if st.last_announce.is_some_and(|t| t.elapsed() < Duration::from_secs(4)) {
                return;
            }
            st.last_announce = Some(Instant::now());
            st.inbox.conversation(chat).map_or_else(|| model::display_number(chat), |c| c.name.clone())
        };
        self.host.announce(&name);
    }

    // ── Conversation ─────────────────────────────────────────────────────────

    /// A conversation opened in the card: its latest messages from the gateway, and the read
    /// receipt when Settings says so.
    pub fn open(self: &Arc<Self>, chat: &str) {
        let key = model::normalized(chat);
        let (client, generation, me, mark) = {
            let mut st = lock(&self.state);
            st.send_error = None;
            st.open_chat = Some(key.clone());
            (st.client.clone(), st.generation, st.me.clone(), st.prefs.mark_read_on_open)
        };
        let Some(client) = client else {
            self.publish();
            return;
        };
        if mark {
            self.mark_read(&key);
        }
        let core = self.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(history) = client.messages(&key, 10, me.as_deref()).await else { return };
            let mut st = lock(&core.state);
            if st.generation != generation {
                return;
            }
            let filters = st.prefs.filters();
            for message in history {
                st.inbox.add(message, &filters, None, true);
            }
            drop(st);
            core.publish();
        });
        self.publish();
    }

    /// The conversation closed (back to the list, Escape, the island folded).
    pub fn close(&self, chat: &str) {
        let key = model::normalized(chat);
        let mut st = lock(&self.state);
        if st.open_chat.as_deref() == Some(key.as_str()) {
            st.open_chat = None;
        }
    }

    /// Marks the chat read here and on WhatsApp (up to its newest received message).
    pub fn mark_read(self: &Arc<Self>, chat: &str) {
        let key = model::normalized(chat);
        let (client, last, had_unread) = {
            let mut st = lock(&self.state);
            let Some(conversation) = st.inbox.conversation(&key) else { return };
            let had_unread = conversation.unread > 0;
            let last = conversation.messages.iter().rev().find(|m| !m.from_me).map(|m| m.id.clone());
            st.inbox.mark_read(&key);
            (st.client.clone(), last, had_unread)
        };
        self.publish();
        // Nothing new: no receipt to send.
        if let (true, Some(client)) = (had_unread, client) {
            tauri::async_runtime::spawn(async move {
                let _ = client.mark_read(&key, last.as_deref()).await;
            });
        }
    }

    /// Sends a reply. Only to a conversation already in the inbox, one at a time, a few a minute.
    /// Ok once the gateway took it; Err is the sentence for the field.
    pub async fn send(self: &Arc<Self>, chat: &str, text: &str) -> Result<(), String> {
        let key = model::normalized(chat);
        let (client, generation, me) = {
            let mut st = lock(&self.state);
            let Some(client) = st.client.clone() else { return Err(describe(&WaError::Unreachable)) };
            if st.sending {
                return Err(String::new());
            }
            if let Some(refusal) = model::check_reply(text, &key, &st.inbox) {
                let message = match refusal {
                    Refusal::TooLong => t("Too long for one message (4,096 characters at most)."),
                    Refusal::Empty | Refusal::NotAConversation => String::new(),
                };
                if refusal == Refusal::TooLong {
                    st.send_error = Some(message.clone());
                }
                drop(st);
                self.publish();
                return Err(message);
            }
            if !st.rate.allow(Instant::now()) {
                let message = t("A few replies a minute at most: wait a moment.");
                st.send_error = Some(message.clone());
                drop(st);
                self.publish();
                return Err(message);
            }
            st.sending = true;
            st.send_error = None;
            (client, st.generation, st.me.clone())
        };
        self.publish();
        let body = text.trim().to_string();
        let result = client.send(&body, &key).await;
        let mark = {
            let mut st = lock(&self.state);
            st.sending = false;
            match &result {
                Ok(id) if st.generation == generation => {
                    // Turned off or restarted meanwhile: the inbox it belonged to is gone.
                    let mut mine = Message::new(
                        &id.clone().unwrap_or_else(|| format!("coucou-{}", random_hex(8))),
                        &key,
                        me.as_deref().unwrap_or(""),
                        "",
                        &body,
                        model::now_secs(),
                    );
                    mine.from_me = true;
                    mine.chat_name = st.inbox.conversation(&key).map(|c| c.name.clone());
                    let filters = st.prefs.filters();
                    st.inbox.add(mine, &filters, None, false);
                    // Replying reads the conversation, whatever the setting.
                    st.inbox.conversation(&key).and_then(|c| c.messages.iter().rev().find(|m| !m.from_me).map(|m| m.id.clone()))
                }
                Ok(_) => None,
                Err(error) => {
                    st.send_error = Some(describe(error));
                    None
                }
            }
        };
        self.publish();
        match result {
            Ok(_) => {
                if let Some(last) = mark {
                    let key = key.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = client.mark_read(&key, Some(&last)).await;
                    });
                }
                Ok(())
            }
            Err(error) => Err(describe(&error)),
        }
    }

    pub fn ignore(self: &Arc<Self>, chat: &str) {
        let key = model::normalized(chat);
        {
            let mut st = lock(&self.state);
            if !st.prefs.ignored.contains(&key) {
                st.prefs.ignored.push(key.clone());
            }
            st.inbox.remove(&key);
            if st.open_chat.as_deref() == Some(key.as_str()) {
                st.open_chat = None;
            }
        }
        self.save_prefs();
        self.publish();
    }

    pub fn unignore(self: &Arc<Self>, chat: &str) {
        let key = model::normalized(chat);
        lock(&self.state).prefs.ignored.retain(|c| model::normalized(c) != key);
        self.save_prefs();
        self.publish();
    }

    // ── Settings ─────────────────────────────────────────────────────────────

    fn save_prefs(&self) {
        let Some(path) = &self.save_to else { return };
        let prefs = lock(&self.state).prefs.clone();
        let dir = path.parent().map(PathBuf::from).unwrap_or_default();
        if let Err(error) = crate::platform::ensure_private_dir(&dir).and_then(|()| super::prefs::save_to(path, &prefs)) {
            log::line(format!("WhatsApp: could not save its settings: {error}"));
        }
    }

    /// Applies what Settings → WhatsApp → Advanced holds. Err is the sentence for the window.
    pub fn set_prefs(self: &Arc<Self>, change: PrefsChange) -> Result<(), String> {
        {
            let mut st = lock(&self.state);
            let candidate = Prefs { url: change.url.trim().to_string(), ..st.prefs.clone() };
            if candidate.gateway_url().is_err() {
                return Err(t("The gateway URL isn’t valid (http://…)"));
            }
            st.prefs.url = candidate.url;
            // A port outside the usual range is ignored (the window's field only offers these).
            if (1024..=65535).contains(&change.webhook_port) {
                st.prefs.webhook_port = change.webhook_port;
            }
            st.prefs.auto_start = change.auto_start;
            st.prefs.mark_read_on_open = change.mark_read_on_open;
            st.prefs.mute_groups = change.mute_groups;
        }
        self.save_prefs();
        self.settings_changed();
        Ok(())
    }

    // ── Install ──────────────────────────────────────────────────────────────

    /// "Install WhatsApp": one click. Progress shows in `status().install`.
    pub fn install(self: &Arc<Self>) {
        {
            let mut st = lock(&self.state);
            if st.install.busy() {
                return;
            }
            st.install = Step::Checking;
        }
        let core = self.clone();
        tauri::async_runtime::spawn(async move { core.run_install().await });
    }

    fn set_step(&self, step: Step) {
        lock(&self.state).install = step;
    }

    pub async fn run_install(self: &Arc<Self>) {
        let Some(target) = Target::current() else {
            self.set_step(Step::Failed {
                message: t("No GOWA build for this computer was found on GitHub. Try again later, or set your own gateway under Advanced."),
            });
            return;
        };
        let (progress, before) = (self.clone(), self.clone());
        let result = installer::install(
            &self.source,
            &self.dir,
            target,
            &move |step| progress.set_step(step),
            // Windows won't replace a running program: the old GOWA gives way first.
            &move || before.process.stop_now(),
        )
        .await;
        match result {
            Ok(version) => {
                {
                    let mut st = lock(&self.state);
                    st.prefs.gowa_version = Some(version.clone());
                    // Back to the default address, started by Coucou.
                    st.prefs.url = String::new();
                    st.prefs.auto_start = true;
                    st.install = Step::Done { version: version.clone() };
                }
                self.save_prefs();
                log::line(format!("WhatsApp: GOWA {version} installed"));
                self.settings_changed();
            }
            Err(message) => {
                log::line("WhatsApp: GOWA install failed");
                self.set_step(Step::Failed { message });
            }
        }
    }

    // ── What the windows see ─────────────────────────────────────────────────

    pub fn snapshot(&self) -> Snapshot {
        let st = lock(&self.state);
        let filters = st.prefs.filters();
        Snapshot {
            link: st.link.code(),
            link_message: st.link.message(),
            me: st.me.clone(),
            conversations: st.inbox.visible(&filters).into_iter().cloned().collect(),
            unread: st.inbox.visible_unread(&filters),
            down: st.failures >= 2 && !matches!(st.link, LinkState::Connected | LinkState::Off),
            sending: st.sending,
            send_error: st.send_error.clone(),
        }
    }

    pub fn status(&self) -> Status {
        let st = lock(&self.state);
        let ignored = st
            .prefs
            .filters()
            .ignored
            .into_iter()
            .map(|id| IgnoredChat { name: st.inbox.conversation(&id).map_or_else(|| model::display_number(&id), |c| c.name.clone()), id })
            .collect();
        Status {
            link: st.link.code(),
            link_message: st.link.message(),
            me: st.me.clone(),
            webhook: st.webhook.code(),
            webhook_line: webhook_sentence(&st),
            qr: st.qr.as_ref().map(|png| format!("data:image/png;base64,{}", model::base64(png))),
            installed: self.dir.join(installer::BINARY_NAME).is_file(),
            version: st.prefs.gowa_version.clone(),
            install: st.install.clone(),
            supported: Target::current().is_some(),
            folder: self.dir.display().to_string(),
            url: st.prefs.gateway_url().map_or_else(|_| st.prefs.url.clone(), |u| u.as_str().trim_end_matches('/').to_string()),
            webhook_port: st.prefs.webhook_port,
            auto_start: st.prefs.auto_start,
            mark_read_on_open: st.prefs.mark_read_on_open,
            mute_groups: st.prefs.mute_groups,
            ignored,
        }
    }

    fn publish(&self) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        self.host.snapshot(self.snapshot());
    }

}

/// The webhook's state in a sentence, for Settings.
fn webhook_sentence(st: &State) -> String {
    match &st.webhook {
        WebhookState::Registered => t("Live: new messages arrive at once."),
        WebhookState::PortBusy => tf("Port {0} is taken: pick another one. Checking every 20 s meanwhile.", &[("0", &st.prefs.webhook_port.to_string())]),
        WebhookState::Refused(why) => tf("The gateway refused Coucou’s address ({0}). Checking every 20 s instead.", &[("0", why)]),
        WebhookState::Off | WebhookState::Listening => t("Checking for new messages every 20 s."),
    }
}

fn random_hex(bytes: usize) -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut buffer = vec![0u8; bytes];
    if SystemRandom::new().fill(&mut buffer).is_err() {
        // The system's generator failing is not something to carry on from: a guessable
        // credential protects nothing.
        panic!("the system's random generator failed");
    }
    model::hex(&buffer)
}
