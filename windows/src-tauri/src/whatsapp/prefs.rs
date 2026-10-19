// What Settings → Integrations → WhatsApp keeps: harmless choices only, in whatsapp.json next to
// settings.json. The gateway's credential and the webhook's secret are not here: they live in
// the Credential Manager (secrets.rs). Whether WhatsApp is on is not here either: it is the
// pill being among the active ones, as for every other integration.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::model::{normalized, Filters};

/// The webhook port Coucou listens on (127.0.0.1 only).
pub const DEFAULT_WEBHOOK_PORT: u16 = 47823;
/// Where GOWA answers when nothing else is set.
pub const DEFAULT_URL: &str = "http://127.0.0.1:3000";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// GOWA's address; empty means the default. Always on this PC (see `gateway_url`).
    pub url: String,
    /// Coucou's webhook port.
    pub webhook_port: u16,
    /// Start (and stop) the GOWA Coucou installed together with Coucou.
    pub auto_start: bool,
    /// Read receipts: sent when a conversation is opened (on), or only when replying (off).
    /// Never on mere reception.
    pub mark_read_on_open: bool,
    /// Groups stay quiet unless the message mentions you.
    pub mute_groups: bool,
    /// Chats never shown (normalised JIDs).
    pub ignored: Vec<String>,
    /// GOWA's version once installed by Coucou.
    pub gowa_version: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            url: String::new(),
            webhook_port: DEFAULT_WEBHOOK_PORT,
            auto_start: true,
            mark_read_on_open: true,
            mute_groups: true,
            ignored: Vec::new(),
            gowa_version: None,
        }
    }
}

impl Prefs {
    pub fn filters(&self) -> Filters {
        Filters { mute_groups: self.mute_groups, ignored: self.ignored.iter().map(|c| normalized(c)).collect::<BTreeSet<_>>() }
    }

    /// The address Coucou talks to. Only http, only this PC, no user name in it.
    pub fn gateway_url(&self) -> Result<reqwest::Url, String> {
        let raw = if self.url.trim().is_empty() { DEFAULT_URL } else { self.url.trim() };
        let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("http://{raw}") };
        let mut url = reqwest::Url::parse(&with_scheme).map_err(|_| "not a valid address".to_string())?;
        if url.scheme() != "http" {
            return Err("the address must start with http://".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("leave the user name and password out of the address".into());
        }
        if !url.host_str().is_some_and(crate::net::is_loopback_host) {
            return Err("the gateway must run on this PC".into());
        }
        // 127.0.0.1 whatever the spelling ("localhost" may resolve to ::1 first on Windows while
        // GOWA listens on IPv4 only).
        let _ = url.set_ip_host(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }

    /// The port GOWA is started on: the address's.
    pub fn gateway_port(&self) -> u16 {
        self.gateway_url().ok().and_then(|u| u.port_or_known_default()).unwrap_or(3000)
    }
}

fn path() -> PathBuf {
    crate::settings::config_dir().join("whatsapp.json")
}

pub fn load() -> Prefs {
    load_from(&path())
}

pub fn load_from(path: &Path) -> Prefs {
    let Ok(bytes) = std::fs::read(path) else { return Prefs::default() };
    let text = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let mut prefs: Prefs = serde_json::from_slice(text).unwrap_or_default();
    if !(1024..=65535).contains(&prefs.webhook_port) {
        prefs.webhook_port = DEFAULT_WEBHOOK_PORT;
    }
    prefs
}

pub fn save_to(path: &Path, prefs: &Prefs) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(prefs).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.part");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_this_pc_and_quiet_for_groups() {
        let p = Prefs::default();
        assert_eq!(p.gateway_url().unwrap().as_str(), "http://127.0.0.1:3000/");
        assert_eq!(p.gateway_port(), 3000);
        assert!(p.mute_groups && p.mark_read_on_open && p.auto_start);
        assert_eq!(p.webhook_port, 47823);
    }

    #[test]
    fn the_gateway_address_is_cleaned_up_and_kept_on_this_pc() {
        let with = |url: &str| Prefs { url: url.into(), ..Prefs::default() };
        assert_eq!(with("localhost:3001").gateway_url().unwrap().as_str(), "http://127.0.0.1:3001/");
        assert_eq!(with("http://127.0.0.1:3002/api?x=1").gateway_url().unwrap().as_str(), "http://127.0.0.1:3002/");
        assert_eq!(with("http://localhost:3003").gateway_port(), 3003);
        assert!(with("http://192.168.1.20:3000").gateway_url().is_err(), "another computer");
        assert!(with("http://example.com").gateway_url().is_err());
        assert!(with("https://127.0.0.1:3000").gateway_url().is_err(), "http only");
        assert!(with("http://user:pw@127.0.0.1:3000").gateway_url().is_err());
        assert!(with("http://").gateway_url().is_err());
    }

    #[test]
    fn prefs_round_trip_and_survive_a_damaged_file() {
        let dir = std::env::temp_dir().join(format!("coucou-wa-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("whatsapp.json");
        assert_eq!(load_from(&file), Prefs::default(), "no file: the defaults");

        let p = Prefs {
            mute_groups: false,
            ignored: vec!["33600000004@c.us".into()],
            gowa_version: Some("9.6.0".into()),
            ..Prefs::default()
        };
        save_to(&file, &p).unwrap();
        assert_eq!(load_from(&file), p);
        assert_eq!(load_from(&file).filters().ignored.into_iter().collect::<Vec<_>>(), ["33600000004@s.whatsapp.net"]);

        std::fs::write(&file, b"{ not json").unwrap();
        assert_eq!(load_from(&file), Prefs::default());
        std::fs::write(&file, br#"{"webhookPort": 80, "muteGroups": false}"#).unwrap();
        let loaded = load_from(&file);
        assert_eq!((loaded.webhook_port, loaded.mute_groups), (DEFAULT_WEBHOOK_PORT, false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
