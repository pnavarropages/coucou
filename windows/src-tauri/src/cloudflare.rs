// Cloudflare, read only: the Workers of an account (their last deployment and the errors of the last 24
// hours) and the state of its domains (zones), for the island's Cloudflare card.
//
// Every request is a GET, with one exception that reads: the errors of the last 24 hours exist only in
// Cloudflare's GraphQL Analytics API, which is queried by POST. Its query is the constant ERRORS_QUERY
// below (a `query`, never a `mutation`); nothing else is ever sent, and a test pins both facts.
// Nothing is created, changed or deleted in the account.
//
// The token is the user's, in the Credential Manager (`cloudflare-api-token`); the account ID is optional
// (`cloudflare-account-id`) and is otherwise found from the zones, or from /accounts.
// The minimum permissions of the token are listed in the *_PERMISSION constants.

use std::collections::HashMap;

use reqwest::Client;
use serde_json::{json, Value};

use crate::i18n::{t, tf};
use crate::net;

/// Credential store entries.
pub const TOKEN_KEY: &str = "cloudflare-api-token";
pub const ACCOUNT_KEY: &str = "cloudflare-account-id";
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// How many Workers get their own deployments lookup (the most recently changed ones).
const DEPLOYMENT_LOOKUPS: usize = 8;
const MAX_ZONES: usize = 50;

/// What the token needs, named as Cloudflare's token editor names it.
const WORKERS_PERMISSION: &str = "Account > Workers Scripts > Read";
const ZONES_PERMISSION: &str = "Zone > Zone > Read";
const ANALYTICS_PERMISSION: &str = "Account > Account Analytics > Read";

/// The only thing ever POSTed: Workers' requests and errors over a time range, per script.
const ERRORS_QUERY: &str = "query($account: String!, $since: Time!, $until: Time!) { viewer { accounts(filter: {accountTag: $account}) { workersInvocationsAdaptive(limit: 1000, filter: {datetime_geq: $since, datetime_leq: $until}) { dimensions { scriptName } sum { requests errors } } } } }";

/// What a poll found.
pub struct Report {
    /// What the card shows: `workers`, `zones`, the totals and, per part, the reason it is missing.
    pub data: Value,
    /// Set when nothing can be shown at all (bad token, no connection): the card's error line.
    pub fatal: Option<String>,
    /// (changes with every deployment, name of the Worker) of the newest deployment, to notice a new one.
    pub newest_deployment: Option<(String, String)>,
}

impl Report {
    fn fatal(message: String) -> Report {
        Report { data: json!({}), fatal: Some(message), newest_deployment: None }
    }
}

#[derive(Debug, PartialEq)]
enum Fail {
    /// Cloudflare does not know the token (401, or the codes it answers a malformed one with).
    BadToken,
    /// The token is valid but lacks a permission (403).
    Forbidden,
    Status(u16),
    Network(String),
    Shape,
}

/// The failure behind an unsuccessful answer: the HTTP status and Cloudflare's own error codes.
fn classify(status: u16, body: &Value) -> Fail {
    let codes: Vec<i64> = body
        .get("errors")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(|e| e.get("code").and_then(Value::as_i64)).collect())
        .unwrap_or_default();
    let permission = status == 403 || codes.contains(&10000);
    let unknown_token = status == 401 || codes.iter().any(|c| matches!(c, 1000 | 6003 | 6111 | 9103 | 9106 | 9107 | 9109));
    if permission && status != 401 {
        Fail::Forbidden
    } else if unknown_token {
        Fail::BadToken
    } else {
        Fail::Status(status)
    }
}

/// The card's line for a part that failed; `permission` is what that part needs.
fn explain(fail: &Fail, permission: &str) -> String {
    match fail {
        Fail::BadToken => t("Invalid API token (401)"),
        Fail::Forbidden => tf("The token needs the permission: {permission}", &[("permission", permission)]),
        Fail::Status(code) => tf("API error {code}", &[("code", &code.to_string())]),
        Fail::Network(error) => tf("No connection: {error}", &[("error", error)]),
        Fail::Shape => t("Unexpected API response."),
    }
}

async fn finish(response: reqwest::Response) -> Result<Value, Fail> {
    let status = response.status().as_u16();
    let bytes = net::read_capped(response, net::MAX_BODY).await.map_err(Fail::Network)?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        if body.is_null() { Err(Fail::Shape) } else { Ok(body) }
    } else {
        Err(classify(status, &body))
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
    finish(response).await
}

/// The one POST: the GraphQL query above, with the account and the time range as variables.
async fn graphql(http: &Client, base: &str, token: &str, account: &str, since: &str, until: &str) -> Result<Value, Fail> {
    let body = json!({ "query": ERRORS_QUERY, "variables": { "account": account, "since": since, "until": until } });
    let response = http
        .post(format!("{base}/graphql"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Fail::Network(e.to_string()))?;
    finish(response).await
}

/// An account or script identifier that is safe to put in a path.
fn is_plain_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 128 && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ── Reading the answers ──────────────────────────────────────────────────────

/// (name, status, paused) of each domain, and the account of the first one.
fn parse_zones(root: &Value) -> (Vec<Value>, Option<String>) {
    let list = root.get("result").and_then(Value::as_array).cloned().unwrap_or_default();
    let account = list
        .first()
        .and_then(|z| z.pointer("/account/id"))
        .and_then(Value::as_str)
        .filter(|id| is_plain_id(id))
        .map(str::to_string);
    let rows = list
        .iter()
        .filter_map(|z| {
            Some(json!({
                "name": z.get("name")?.as_str()?,
                "status": z.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "paused": z.get("paused").and_then(Value::as_bool).unwrap_or(false),
            }))
        })
        .collect();
    (rows, account)
}

fn parse_accounts(root: &Value) -> Option<String> {
    root.pointer("/result/0/id").and_then(Value::as_str).filter(|id| is_plain_id(id)).map(str::to_string)
}

/// (name, last modified) of each Worker, the most recently modified first.
fn parse_scripts(root: &Value) -> Vec<(String, String)> {
    let mut scripts: Vec<(String, String)> = root
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s.get("id").and_then(Value::as_str).filter(|n| is_plain_id(n))?;
            let modified = s.get("modified_on").and_then(Value::as_str).unwrap_or("");
            Some((name.to_string(), modified.to_string()))
        })
        .collect();
    scripts.sort_by(|a, b| b.1.cmp(&a.1));
    scripts
}

/// (id, time) of the newest deployment of a Worker, whatever order Cloudflare lists them in.
fn parse_latest_deployment(root: &Value) -> Option<(String, String)> {
    let result = root.get("result")?;
    let list = result.get("deployments").or_else(|| result.get("items")).or(Some(result))?.as_array()?;
    list.iter()
        .filter_map(|d| Some((d.get("id")?.as_str()?.to_string(), d.get("created_on")?.as_str()?.to_string())))
        .max_by(|a, b| a.1.cmp(&b.1))
}

/// Requests and errors per Worker over the range. A GraphQL answer carries its refusals in `errors`
/// with a 200: no `viewer` then means the token may not read analytics.
fn parse_invocations(root: &Value) -> Result<HashMap<String, (i64, i64)>, Fail> {
    let Some(rows) = root.pointer("/data/viewer/accounts/0/workersInvocationsAdaptive").and_then(Value::as_array) else {
        let refused = root.get("errors").and_then(Value::as_array).is_some_and(|e| !e.is_empty());
        return Err(if refused { Fail::Forbidden } else { Fail::Shape });
    };
    let mut per_script: HashMap<String, (i64, i64)> = HashMap::new();
    for row in rows {
        let Some(name) = row.pointer("/dimensions/scriptName").and_then(Value::as_str) else { continue };
        let count = |key: &str| row.pointer(&format!("/sum/{key}")).and_then(Value::as_f64).unwrap_or(0.0) as i64;
        let entry = per_script.entry(name.to_string()).or_default();
        entry.0 += count("requests");
        entry.1 += count("errors");
    }
    Ok(per_script)
}

// ── The poll ─────────────────────────────────────────────────────────────────

/// Reads the account. `base` is Cloudflare's API (tests point it at a local server), `now` the time in
/// seconds since the epoch.
pub async fn poll(http: &Client, base: &str, token: &str, account: Option<&str>, now: u64) -> Report {
    let mut account_id = account.map(str::trim).filter(|a| is_plain_id(a)).map(str::to_string);

    // Domains first: they also tell which account the token belongs to.
    let filter = account_id.as_deref().map(|a| format!("&account.id={a}")).unwrap_or_default();
    let mut zones: Vec<Value> = Vec::new();
    let mut zones_error: Option<String> = None;
    match get(http, base, token, &format!("/zones?per_page={MAX_ZONES}{filter}")).await {
        Ok(root) => {
            let (rows, found) = parse_zones(&root);
            zones = rows;
            account_id = account_id.or(found);
        }
        Err(fail @ (Fail::BadToken | Fail::Network(_))) => return Report::fatal(explain(&fail, ZONES_PERMISSION)),
        Err(fail) => zones_error = Some(explain(&fail, ZONES_PERMISSION)),
    }
    if account_id.is_none() {
        match get(http, base, token, "/accounts?per_page=5").await {
            Ok(root) => account_id = parse_accounts(&root),
            Err(fail @ (Fail::BadToken | Fail::Network(_))) => return Report::fatal(explain(&fail, ZONES_PERMISSION)),
            Err(_) => {}
        }
    }

    let mut workers: Vec<Value> = Vec::new();
    let mut workers_total = 0usize;
    let mut workers_error: Option<String> = None;
    let mut analytics_error: Option<String> = None;
    let mut errors_24h: Option<i64> = None;
    let mut newest: Option<(String, String)> = None;

    match account_id.as_deref() {
        None => workers_error = Some(t("Add the Cloudflare account ID in Settings.")),
        Some(account) => match get(http, base, token, &format!("/accounts/{account}/workers/scripts")).await {
            Err(fail @ (Fail::BadToken | Fail::Network(_))) => return Report::fatal(explain(&fail, WORKERS_PERMISSION)),
            Err(fail) => workers_error = Some(explain(&fail, WORKERS_PERMISSION)),
            Ok(root) => {
                let scripts = parse_scripts(&root);
                workers_total = scripts.len();

                let (since, until) = (net::iso_utc(now.saturating_sub(86_400)), net::iso_utc(now));
                let invocations = match graphql(http, base, token, account, &since, &until).await {
                    Ok(answer) => parse_invocations(&answer),
                    Err(fail) => Err(fail),
                };
                let invocations = match invocations {
                    Ok(map) => {
                        errors_24h = Some(map.values().map(|(_, errors)| errors).sum());
                        map
                    }
                    Err(fail) => {
                        analytics_error = Some(explain(&fail, ANALYTICS_PERMISSION));
                        HashMap::new()
                    }
                };

                for (name, modified) in scripts.iter().take(DEPLOYMENT_LOOKUPS) {
                    let path = format!("/accounts/{account}/workers/scripts/{name}/deployments");
                    let latest = match get(http, base, token, &path).await {
                        Ok(answer) => parse_latest_deployment(&answer),
                        Err(_) => None,
                    };
                    // Without a deployment record, the last change is the best "last deployment".
                    let (deployment_id, deployed_at) = match latest {
                        Some((id, at)) => (id, at),
                        None => (modified.clone(), modified.clone()),
                    };
                    let seen = invocations.get(name);
                    workers.push(json!({
                        "name": name,
                        "deployedAt": if deployed_at.is_empty() { Value::Null } else { json!(deployed_at) },
                        "deploymentId": deployment_id,
                        "requests": seen.map(|s| s.0),
                        "errors": seen.map(|s| s.1),
                    }));
                }
                workers.sort_by(|a, b| {
                    let at = |w: &Value| w.get("deployedAt").and_then(Value::as_str).unwrap_or("").to_string();
                    at(b).cmp(&at(a))
                });
                newest = workers.first().and_then(|w| {
                    let name = w.get("name")?.as_str()?;
                    let at = w.get("deployedAt")?.as_str()?;
                    Some((format!("{name}@{at}"), name.to_string()))
                });
            }
        },
    }

    Report {
        data: json!({
            "workers": workers,
            "workersTotal": workers_total,
            "errors24h": errors_24h,
            "zones": zones,
            "workersError": workers_error,
            "analyticsError": analytics_error,
            "zonesError": zones_error,
        }),
        fatal: None,
        newest_deployment: newest,
    }
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

    fn run(base: &str, account: Option<&str>) -> Report {
        let _turn = crate::net::tests::loopback_turn();
        let http = Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().unwrap();
        block_on(poll(&http, base, "tok-123", account, NOW))
    }

    const ACCOUNT: &str = "023e105f4ecef8ad9ca31a8372d0c353";

    fn zones() -> String {
        json!({"success": true, "result": [
            {"id": "z1", "name": "orbitra-studio.com", "status": "active", "paused": false, "account": {"id": ACCOUNT, "name": "Orbitra"}},
            {"id": "z2", "name": "nuevo.example", "status": "pending", "paused": false, "account": {"id": ACCOUNT}},
            {"id": "z3", "name": "parado.example", "status": "active", "paused": true, "account": {"id": ACCOUNT}},
        ]})
        .to_string()
    }

    fn scripts() -> String {
        json!({"success": true, "result": [
            {"id": "api", "modified_on": "2026-10-10T08:00:00Z"},
            {"id": "relay", "modified_on": "2026-10-18T09:30:00Z"},
            {"id": "cron", "modified_on": "2026-09-01T00:00:00Z"},
        ]})
        .to_string()
    }

    fn deployments(id: &str, at: &str) -> String {
        json!({"success": true, "result": {"deployments": [
            {"id": "old", "created_on": "2026-01-01T00:00:00Z"},
            {"id": id, "created_on": at},
        ]}})
        .to_string()
    }

    fn analytics() -> String {
        json!({"data": {"viewer": {"accounts": [{"workersInvocationsAdaptive": [
            {"dimensions": {"scriptName": "api"}, "sum": {"requests": 1200, "errors": 7}},
            {"dimensions": {"scriptName": "api"}, "sum": {"requests": 300, "errors": 1}},
            {"dimensions": {"scriptName": "relay"}, "sum": {"requests": 50, "errors": 0}},
        ]}]}}, "errors": null})
        .to_string()
    }

    fn full_routes() -> Vec<(&'static str, u16, String)> {
        vec![
            ("GET /zones", 200, zones()),
            ("GET /accounts/023e105f4ecef8ad9ca31a8372d0c353/workers/scripts/api/deployments", 200, deployments("d-api", "2026-10-12T10:00:00Z")),
            ("GET /accounts/023e105f4ecef8ad9ca31a8372d0c353/workers/scripts/relay/deployments", 200, deployments("d-relay", "2026-10-18T09:31:00Z")),
            ("GET /accounts/023e105f4ecef8ad9ca31a8372d0c353/workers/scripts/cron/deployments", 404, "{}".into()),
            ("GET /accounts/023e105f4ecef8ad9ca31a8372d0c353/workers/scripts", 200, scripts()),
            ("POST /graphql", 200, analytics()),
        ]
    }

    #[test]
    fn it_shows_workers_errors_and_domains_found_with_the_zones_account() {
        let (base, seen) = serve_routes(full_routes());
        let report = run(&base, None);
        assert!(report.fatal.is_none());
        let d = &report.data;
        assert_eq!(d["workersTotal"], 3);
        assert_eq!(d["errors24h"], 8);
        let names: Vec<&str> = d["workers"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["relay", "api", "cron"], "the newest deployment first");
        assert_eq!(d["workers"][0]["deploymentId"], "d-relay");
        assert_eq!(d["workers"][0]["deployedAt"], "2026-10-18T09:31:00Z");
        assert_eq!((d["workers"][1]["requests"].clone(), d["workers"][1]["errors"].clone()), (json!(1500), json!(8)));
        // No deployment record: the last change stands in for it, and no traffic is not an error count.
        assert_eq!(d["workers"][2]["deployedAt"], "2026-09-01T00:00:00Z");
        assert_eq!(d["workers"][2]["errors"], Value::Null);
        let zones = d["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 3);
        assert_eq!((zones[1]["name"].clone(), zones[1]["status"].clone()), (json!("nuevo.example"), json!("pending")));
        assert_eq!(zones[2]["paused"], true);
        assert!(d["workersError"].is_null() && d["analyticsError"].is_null() && d["zonesError"].is_null());
        assert_eq!(report.newest_deployment, Some(("relay@2026-10-18T09:31:00Z".into(), "relay".into())));

        let log = seen.lock().unwrap();
        assert!(log.iter().all(|r| r.to_ascii_lowercase().contains("authorization: bearer tok-123")));
        assert!(log.iter().all(|r| !r.contains("/accounts?per_page")), "the zones already named the account");
    }

    #[test]
    fn it_never_writes_and_the_only_post_is_the_analytics_query() {
        let (base, seen) = serve_routes(full_routes());
        let _ = run(&base, Some(ACCOUNT));
        let log = seen.lock().unwrap();
        assert!(!log.is_empty());
        for request in log.iter() {
            let line = request.lines().next().unwrap();
            assert!(line.starts_with("GET ") || line.starts_with("POST /graphql "), "{line}");
        }
        let post = log.iter().find(|r| r.starts_with("POST ")).expect("the analytics query");
        let body = post.split("\r\n\r\n").nth(1).unwrap();
        let sent: Value = serde_json::from_str(body).unwrap();
        assert_eq!(sent["query"], ERRORS_QUERY);
        assert_eq!(sent["variables"]["account"], ACCOUNT);
        assert_eq!(sent["variables"]["since"], "2023-11-13T22:13:20Z");
        assert_eq!(sent["variables"]["until"], "2023-11-14T22:13:20Z");
        assert!(ERRORS_QUERY.starts_with("query") && !ERRORS_QUERY.contains("mutation"));
    }

    #[test]
    fn a_configured_account_filters_the_zones_and_is_used_as_given() {
        let (base, seen) = serve_routes(full_routes());
        let report = run(&base, Some(&format!(" {ACCOUNT} ")));
        assert!(report.fatal.is_none());
        let log = seen.lock().unwrap();
        assert!(log[0].starts_with(&format!("GET /zones?per_page=50&account.id={ACCOUNT} ")), "{}", log[0]);
        // Something that is not an identifier is ignored, never put in a path.
        drop(log);
        let (base, seen) = serve_routes(full_routes());
        let _ = run(&base, Some("../../user"));
        assert!(seen.lock().unwrap()[0].starts_with("GET /zones?per_page=50 "));
    }

    #[test]
    fn an_unknown_token_stops_at_the_first_answer() {
        let body = json!({"success": false, "errors": [{"code": 1000, "message": "Invalid API Token"}]}).to_string();
        let (base, seen) = serve_routes(vec![("GET /zones", 401, body)]);
        let report = run(&base, None);
        assert_eq!(report.fatal.as_deref(), Some("Invalid API token (401)"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        // A token that is not even shaped like one: Cloudflare says 400 with its own codes.
        let body = json!({"errors": [{"code": 6003}, {"code": 6111}]}).to_string();
        let (base, _) = serve_routes(vec![("GET /zones", 400, body)]);
        assert_eq!(run(&base, None).fatal.as_deref(), Some("Invalid API token (401)"));
    }

    #[test]
    fn a_token_without_workers_access_still_shows_the_domains_and_names_the_permission() {
        let denied = json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}).to_string();
        let mut routes = full_routes();
        routes.retain(|(p, _, _)| !p.ends_with("/workers/scripts") && !p.contains("deployments"));
        routes.push(("GET /accounts/023e105f4ecef8ad9ca31a8372d0c353/workers/scripts", 403, denied));
        let (base, _) = serve_routes(routes);
        let report = run(&base, None);
        assert!(report.fatal.is_none());
        assert_eq!(report.data["zones"].as_array().unwrap().len(), 3);
        assert_eq!(report.data["workers"], json!([]));
        assert_eq!(
            report.data["workersError"],
            "The token needs the permission: Account > Workers Scripts > Read"
        );
        assert!(report.newest_deployment.is_none());
    }

    #[test]
    fn without_analytics_access_the_workers_show_with_no_error_counts() {
        let refused = json!({"data": null, "errors": [{"message": "not authorized to access this account's analytics"}]}).to_string();
        let mut routes = full_routes();
        routes.retain(|(p, _, _)| *p != "POST /graphql");
        routes.push(("POST /graphql", 200, refused));
        let (base, _) = serve_routes(routes);
        let report = run(&base, None);
        let d = &report.data;
        assert_eq!(d["workers"].as_array().unwrap().len(), 3);
        assert_eq!(d["errors24h"], Value::Null);
        assert_eq!(d["workers"][0]["errors"], Value::Null);
        assert_eq!(d["analyticsError"], "The token needs the permission: Account > Account Analytics > Read");
        assert!(d["workersError"].is_null());
    }

    #[test]
    fn with_no_account_anywhere_it_says_to_enter_it() {
        let (base, _) = serve_routes(vec![
            ("GET /zones", 200, json!({"result": []}).to_string()),
            ("GET /accounts", 403, "{}".into()),
        ]);
        let report = run(&base, None);
        assert!(report.fatal.is_none());
        assert_eq!(report.data["workersError"], "Add the Cloudflare account ID in Settings.");
        assert_eq!(report.data["zones"], json!([]));
    }

    #[test]
    fn the_account_comes_from_the_accounts_list_when_there_are_no_zones() {
        let accounts = json!({"result": [{"id": ACCOUNT, "name": "Orbitra"}]}).to_string();
        let mut routes = full_routes();
        routes.retain(|(p, _, _)| *p != "GET /zones");
        routes.insert(0, ("GET /zones", 200, json!({"result": []}).to_string()));
        routes.insert(1, ("GET /accounts?per_page=5", 200, accounts));
        let (base, _) = serve_routes(routes);
        let report = run(&base, None);
        assert_eq!(report.data["workersTotal"], 3);
    }

    #[test]
    fn answers_are_read_defensively() {
        assert_eq!(classify(403, &json!({})), Fail::Forbidden);
        assert_eq!(classify(403, &json!({"errors": [{"code": 10000}]})), Fail::Forbidden);
        assert_eq!(classify(401, &json!({})), Fail::BadToken);
        assert_eq!(classify(400, &json!({"errors": [{"code": 6111}]})), Fail::BadToken);
        assert_eq!(classify(500, &json!({})), Fail::Status(500));
        assert!(parse_zones(&json!({})).0.is_empty());
        assert!(parse_scripts(&json!({"result": [{"id": "../x"}, {"nope": 1}]})).is_empty());
        assert_eq!(parse_latest_deployment(&json!({"result": {"deployments": []}})), None);
        assert_eq!(
            parse_latest_deployment(&json!({"result": {"items": [{"id": "a", "created_on": "2026-01-02"}, {"id": "b", "created_on": "2026-01-03"}]}})),
            Some(("b".into(), "2026-01-03".into()))
        );
        assert!(matches!(parse_invocations(&json!({"data": null, "errors": [{"message": "x"}]})), Err(Fail::Forbidden)));
        assert!(matches!(parse_invocations(&json!({})), Err(Fail::Shape)));
        assert!(is_plain_id("my-worker_2.prod") && !is_plain_id("a/b") && !is_plain_id("a b") && !is_plain_id(""));
        assert!(crate::secrets::KNOWN_KEYS.contains(&TOKEN_KEY) && crate::secrets::KNOWN_KEYS.contains(&ACCOUNT_KEY));
    }
}
