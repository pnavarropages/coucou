// Hetzner Cloud, read only: the servers of the project the token belongs to — their state (running, off…),
// public address and, for the running ones, the CPU and network use of the last minutes — for the island's
// Hetzner card.
//
// Every request is a GET. Nothing is created, rebooted, powered off or deleted: a read-only project token
// (Security > API tokens > permission "Read") is all it needs, and is what the settings recommend.
// The token is the user's, in the Credential Manager (`hetzner-api-token`). A Hetzner token is bound to one
// project, so this shows that project's servers.

use std::collections::HashMap;
use std::sync::Mutex;

use reqwest::Client;
use serde_json::{json, Value};

use crate::i18n::{t, tf};
use crate::net;

/// Credential store entry.
pub const TOKEN_KEY: &str = "hetzner-api-token";
pub const API_BASE: &str = "https://api.hetzner.cloud/v1";

const MAX_SERVERS: usize = 50;
/// Servers that get a metrics lookup (running ones, in the card's order): one request each.
const METRICS_LOOKUPS: usize = 10;
/// The window the metrics are read from, and the step of their samples.
const METRICS_WINDOW_SECS: u64 = 600;
const METRICS_STEP_SECS: u64 = 60;
const PERMISSION: &str = "Read";

/// What a poll found.
pub struct Report {
    /// What the card shows: `servers` and the totals, or the reason metrics are missing.
    pub data: Value,
    /// Set when nothing can be shown at all (bad token, no connection): the card's error line.
    pub fatal: Option<String>,
    /// (id, name, status) of every server, to notice one that went off or came back.
    pub states: Vec<(i64, String, String)>,
}

#[derive(Debug, PartialEq)]
enum Fail {
    BadToken,
    Forbidden,
    Status(u16),
    Network(String),
    Shape,
}

fn classify(status: u16, body: &Value) -> Fail {
    let code = body.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
    match (status, code) {
        (401, _) | (_, "unauthorized") => Fail::BadToken,
        (403, _) | (_, "forbidden") => Fail::Forbidden,
        _ => Fail::Status(status),
    }
}

fn explain(fail: &Fail) -> String {
    match fail {
        Fail::BadToken => t("Invalid API token (401)"),
        Fail::Forbidden => tf("The token needs the permission: {permission}", &[("permission", PERMISSION)]),
        Fail::Status(429) => t("Hetzner is limiting requests. Try again in a while."),
        Fail::Status(code) => tf("API error {code}", &[("code", &code.to_string())]),
        Fail::Network(error) => tf("No connection: {error}", &[("error", error)]),
        Fail::Shape => t("Unexpected API response."),
    }
}

async fn get(http: &Client, base: &str, token: &str, path: &str) -> Result<Value, Fail> {
    let response = http
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| Fail::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let bytes = net::read_capped(response, net::MAX_BODY).await.map_err(Fail::Network)?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        if body.is_null() { Err(Fail::Shape) } else { Ok(body) }
    } else {
        Err(classify(status, &body))
    }
}

// ── Reading the answers ──────────────────────────────────────────────────────

/// A server as the card needs it; the metrics are added after.
fn parse_server(s: &Value) -> Option<Value> {
    let id = s.get("id")?.as_i64()?;
    let ip = s.pointer("/public_net/ipv4/ip").and_then(Value::as_str).filter(|ip| !ip.is_empty());
    let place = s.pointer("/datacenter/location/city").or_else(|| s.pointer("/datacenter/name")).and_then(Value::as_str);
    Some(json!({
        "id": id,
        "name": s.get("name")?.as_str()?,
        "status": s.get("status").and_then(Value::as_str).unwrap_or("unknown"),
        "ip": ip,
        "type": s.pointer("/server_type/name").and_then(Value::as_str),
        "location": place,
        "cpu": Value::Null,
        "netIn": Value::Null,
        "netOut": Value::Null,
    }))
}

/// The newest sample of a series (`{"values": [[time, "value"], …]}`); Hetzner sends the numbers as text.
fn last_value(series: &Value) -> Option<f64> {
    series
        .get("values")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|pair| pair.get(1).and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64())))
        .filter(|v| v.is_finite())
}

/// (cpu %, bytes/s in, bytes/s out) of the newest samples.
fn parse_metrics(root: &Value) -> (Option<f64>, Option<f64>, Option<f64>) {
    let series = |name: &str| root.pointer("/metrics/time_series").and_then(|t| t.get(name)).and_then(last_value);
    (series("cpu"), series("network.0.bandwidth.in"), series("network.0.bandwidth.out"))
}

/// The card's order: what is not running first (it needs a look), then by name.
fn order(servers: &mut [Value]) {
    let key = |s: &Value| {
        let running = s.get("status").and_then(Value::as_str) == Some("running");
        (running, s.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase())
    };
    servers.sort_by_key(key);
}

// ── The poll ─────────────────────────────────────────────────────────────────

/// Reads the project. `base` is Hetzner's API (tests point it at a local server), `now` the time in seconds
/// since the epoch.
pub async fn poll(http: &Client, base: &str, token: &str, now: u64) -> Report {
    let root = match get(http, base, token, &format!("/servers?per_page={MAX_SERVERS}")).await {
        Ok(root) => root,
        Err(fail) => {
            return Report { data: json!({}), fatal: Some(explain(&fail)), states: Vec::new() };
        }
    };
    let mut servers: Vec<Value> = root
        .get("servers")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_server).collect())
        .unwrap_or_default();
    order(&mut servers);

    let states = servers
        .iter()
        .filter_map(|s| Some((s.get("id")?.as_i64()?, s.get("name")?.as_str()?.to_string(), s.get("status")?.as_str()?.to_string())))
        .collect();

    let (start, end) = (net::iso_utc(now.saturating_sub(METRICS_WINDOW_SECS)), net::iso_utc(now));
    let mut metrics_error: Option<String> = None;
    let mut looked_up = 0;
    for server in servers.iter_mut() {
        if server.get("status").and_then(Value::as_str) != Some("running") {
            continue;
        }
        if looked_up == METRICS_LOOKUPS {
            break;
        }
        looked_up += 1;
        let id = server.get("id").and_then(Value::as_i64).unwrap_or(0);
        let path = format!("/servers/{id}/metrics?type=cpu,network&start={start}&end={end}&step={METRICS_STEP_SECS}");
        match get(http, base, token, &path).await {
            Ok(answer) => {
                let (cpu, net_in, net_out) = parse_metrics(&answer);
                server["cpu"] = json!(cpu);
                server["netIn"] = json!(net_in);
                server["netOut"] = json!(net_out);
            }
            Err(fail) => {
                metrics_error.get_or_insert_with(|| explain(&fail));
            }
        }
    }

    let running = servers.iter().filter(|s| s.get("status").and_then(Value::as_str) == Some("running")).count();
    Report {
        data: json!({ "servers": servers, "total": servers.len(), "running": running, "metricsError": metrics_error }),
        fatal: None,
        states,
    }
}

// ── Noticing a change ────────────────────────────────────────────────────────

/// A server's state when it moves between the two that matter. Booting, stopping or migrating say nothing yet.
fn settled(status: &str) -> bool {
    matches!(status, "running" | "off")
}

/// (name, new status) of each server that is now running or off and was in another state before. The first
/// look only fills the memory, like the other pollers: nothing is announced for what was already so.
fn changes(previous: &mut Option<HashMap<i64, String>>, current: &[(i64, String, String)]) -> Vec<(String, String)> {
    let now: HashMap<i64, String> = current.iter().map(|(id, _, status)| (*id, status.clone())).collect();
    let before = previous.replace(now);
    let Some(before) = before else { return Vec::new() };
    current
        .iter()
        .filter(|(id, _, status)| settled(status) && before.get(id).is_some_and(|old| old != status))
        .map(|(_, name, status)| (name.clone(), status.clone()))
        .collect()
}

static LAST_STATES: Mutex<Option<HashMap<i64, String>>> = Mutex::new(None);

/// `changes` against what the previous poll saw.
pub fn status_changes(current: &[(i64, String, String)]) -> Vec<(String, String)> {
    changes(&mut LAST_STATES.lock().unwrap(), current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::serve_routes;
    use std::time::Duration;

    const NOW: u64 = 1_700_000_000;

    fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn run(base: &str) -> Report {
        let _turn = crate::net::tests::loopback_turn();
        let http = Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().unwrap();
        block_on(poll(&http, base, "tok-123", NOW))
    }

    fn servers() -> String {
        json!({"servers": [
            {"id": 11, "name": "web", "status": "running", "public_net": {"ipv4": {"ip": "203.0.113.10"}, "ipv6": {"ip": "2001:db8::/64"}},
             "server_type": {"name": "cx22"}, "datacenter": {"name": "fsn1-dc14", "location": {"city": "Falkenstein"}}},
            {"id": 12, "name": "Backup", "status": "off", "public_net": {"ipv4": {"ip": "203.0.113.11"}}, "server_type": {"name": "cx11"}},
            {"id": 13, "name": "api", "status": "running", "public_net": {"ipv4": null}, "datacenter": {"name": "nbg1-dc3"}},
        ], "meta": {"pagination": {"page": 1}}})
        .to_string()
    }

    fn metrics(cpu: &str) -> String {
        json!({"metrics": {"start": "x", "end": "y", "step": 60, "time_series": {
            "cpu": {"values": [[1699999900, "1.0"], [1699999960, cpu]]},
            "network.0.bandwidth.in": {"values": [[1699999900, "100.5"], [1699999960, "2048"]]},
            "network.0.bandwidth.out": {"values": [[1699999960, "512"]]},
            "network.0.pps.in": {"values": [[1699999960, "3"]]},
        }}})
        .to_string()
    }

    #[test]
    fn it_lists_servers_with_state_address_and_use() {
        let (base, seen) = serve_routes(vec![
            ("GET /servers/11/metrics", 200, metrics("7.25")),
            ("GET /servers/13/metrics", 200, json!({"metrics": {"time_series": {}}}).to_string()),
            ("GET /servers?", 200, servers()),
        ]);
        let report = run(&base);
        assert!(report.fatal.is_none());
        let d = &report.data;
        assert_eq!((d["total"].clone(), d["running"].clone()), (json!(3), json!(2)));
        let names: Vec<&str> = d["servers"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Backup", "api", "web"], "what is off first, then by name");
        let web = &d["servers"][2];
        assert_eq!(web["ip"], "203.0.113.10");
        assert_eq!((web["type"].clone(), web["location"].clone()), (json!("cx22"), json!("Falkenstein")));
        assert_eq!((web["cpu"].clone(), web["netIn"].clone(), web["netOut"].clone()), (json!(7.25), json!(2048.0), json!(512.0)));
        assert_eq!(d["servers"][0]["ip"], "203.0.113.11");
        assert_eq!(d["servers"][0]["cpu"], Value::Null, "an off server has no use to read");
        assert_eq!(d["servers"][1]["ip"], Value::Null);
        assert_eq!(d["servers"][1]["location"], "nbg1-dc3");
        assert_eq!(d["servers"][1]["cpu"], Value::Null, "no samples yet");
        assert!(d["metricsError"].is_null());
        assert_eq!(report.states.len(), 3);

        let log = seen.lock().unwrap();
        assert_eq!(log.len(), 3, "the list, then metrics for the two running servers only");
        assert!(log.iter().all(|r| r.starts_with("GET ") && r.to_ascii_lowercase().contains("authorization: bearer tok-123")));
        let metrics_line = log.iter().find(|r| r.contains("/metrics")).unwrap().lines().next().unwrap().to_string();
        assert!(metrics_line.contains("type=cpu,network&start=2023-11-14T22:03:20Z&end=2023-11-14T22:13:20Z&step=60"), "{metrics_line}");
    }

    #[test]
    fn a_wrong_token_or_one_without_read_access_is_said_plainly() {
        let body = json!({"error": {"code": "unauthorized", "message": "unable to authenticate"}}).to_string();
        let (base, seen) = serve_routes(vec![("GET /servers", 401, body)]);
        assert_eq!(run(&base).fatal.as_deref(), Some("Invalid API token (401)"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        let body = json!({"error": {"code": "forbidden", "message": "insufficient permissions"}}).to_string();
        let (base, _) = serve_routes(vec![("GET /servers", 403, body)]);
        assert_eq!(run(&base).fatal.as_deref(), Some("The token needs the permission: Read"));
        let (base, _) = serve_routes(vec![("GET /servers", 429, json!({"error": {"code": "rate_limit_exceeded"}}).to_string())]);
        assert_eq!(run(&base).fatal.as_deref(), Some("Hetzner is limiting requests. Try again in a while."));
        let (base, _) = serve_routes(vec![("GET /servers", 503, "{}".into())]);
        assert_eq!(run(&base).fatal.as_deref(), Some("API error 503"));
    }

    #[test]
    fn missing_metrics_do_not_hide_the_servers() {
        let (base, _) = serve_routes(vec![
            ("GET /servers/11/metrics", 403, json!({"error": {"code": "forbidden"}}).to_string()),
            ("GET /servers/13/metrics", 403, json!({"error": {"code": "forbidden"}}).to_string()),
            ("GET /servers?", 200, servers()),
        ]);
        let report = run(&base);
        assert!(report.fatal.is_none());
        assert_eq!(report.data["servers"].as_array().unwrap().len(), 3);
        assert_eq!(report.data["metricsError"], "The token needs the permission: Read");
    }

    #[test]
    fn an_empty_project_is_not_an_error() {
        let (base, _) = serve_routes(vec![("GET /servers?", 200, json!({"servers": []}).to_string())]);
        let report = run(&base);
        assert!(report.fatal.is_none());
        assert_eq!((report.data["total"].clone(), report.data["running"].clone()), (json!(0), json!(0)));
    }

    #[test]
    fn a_server_going_off_or_coming_back_is_announced_once() {
        let mut memory = None;
        let s = |id: i64, name: &str, status: &str| (id, name.to_string(), status.to_string());
        // The first look announces nothing.
        assert!(changes(&mut memory, &[s(1, "web", "running"), s(2, "db", "off")]).is_empty());
        // Nothing moved.
        assert!(changes(&mut memory, &[s(1, "web", "running"), s(2, "db", "off")]).is_empty());
        // web went off, db came back, a new server is not a change.
        let moved = changes(&mut memory, &[s(1, "web", "off"), s(2, "db", "running"), s(3, "new", "running")]);
        assert_eq!(moved, vec![("web".to_string(), "off".to_string()), ("db".to_string(), "running".to_string())]);
        // Booting and stopping are not settled yet.
        assert!(changes(&mut memory, &[s(1, "web", "starting"), s(2, "db", "stopping"), s(3, "new", "running")]).is_empty());
        assert_eq!(changes(&mut memory, &[s(1, "web", "running"), s(2, "db", "off"), s(3, "new", "running")]).len(), 2);
    }

    #[test]
    fn answers_are_read_defensively() {
        assert_eq!(classify(401, &json!({})), Fail::BadToken);
        assert_eq!(classify(403, &json!({})), Fail::Forbidden);
        assert_eq!(classify(500, &json!({})), Fail::Status(500));
        assert_eq!(last_value(&json!({"values": []})), None);
        assert_eq!(last_value(&json!({"values": [[1, "4"], [2, null]]})), Some(4.0));
        assert_eq!(last_value(&json!({"values": [[1, 4.5]]})), Some(4.5));
        assert!(parse_server(&json!({"id": "x"})).is_none());
        assert!(crate::secrets::KNOWN_KEYS.contains(&TOKEN_KEY));
    }
}
