//! Calendar sync against emulate's Microsoft Graph (`UPLINK_EMULATE`, set
//! by the dev shell and `nix flake check`). There is no real account to
//! run it against yet (SPEC §15.12).

use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, process::Stdio, sync::Arc, time::Duration};
use uplink_calendar::{service::Calendar, sync::CalendarSync};
use uplink_config::{M365AccountConfig, M365OAuthConfig};
use uplink_email::{
    m365::{api::Graph, oauth},
    oauth::Credentials,
};
use uplink_protocol::{calendar::CalendarEvent, ServerResponse};
use uplink_realtime::Hub;
use uplink_storage::seal::MasterKey;
use uplink_tx_log::{Dispatcher, Registry};

const DAY: i64 = 86_400_000;
/// "Now", for the tests: Monday 2026-11-09, 00:00 UTC.
const NOW: i64 = 1_794_182_400_000;

struct Setup {
    dir: tempfile::TempDir,
    _emulate: tokio::process::Child,
    base: String,
    http: reqwest::Client,
    alice: Credentials,
    sync: CalendarSync,
    calendar: Calendar,
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (base, emulate) = start_emulate(dir.path()).await;
        let key_path = dir.path().join("master.key");
        let key = Arc::new(MasterKey::open(&key_path).unwrap());
        let account = M365AccountConfig {
            id: "test".into(),
            address: "alice@example.com".into(),
            oauth: M365OAuthConfig {
                client_id: "uplink-test".into(),
                redirect_uri: "http://127.0.0.1/redirect".into(),
                authority: format!("{base}/common"),
                resource: base.clone(),
                token_file: dir.path().join("accounts/test.token"),
            },
        };
        let http = reqwest::Client::new();
        let code = oauth::pasted_code(&sign_in(&account).await).unwrap();
        oauth::redeem(&http, &account, &key, &code).await.unwrap();
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            Registry::new(),
            Arc::new(Hub::new(64)),
        );
        let sync = CalendarSync::new(
            Graph::of(&account, key.clone(), http.clone()),
            account.id.clone(),
            dispatcher,
            uplink_storage::open_reader(dir.path()).unwrap(),
            Duration::from_millis(100),
        );
        Self {
            alice: Credentials::new(http.clone(), oauth::grant(&account), key),
            calendar: Calendar::new(uplink_storage::open_reader(dir.path()).unwrap()),
            sync,
            http,
            base,
            dir,
            _emulate: emulate,
        }
    }

    /// Calls Graph as Alice.
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .http
            .request(method, format!("{}/v1.0/me{path}", self.base))
            .bearer_auth(self.alice.access_token().await.unwrap());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap_or(Value::Null)
    }

    async fn create(&self, event: Value) -> String {
        let created = self
            .call(reqwest::Method::POST, "/events", Some(event))
            .await;
        created["id"].as_str().unwrap().to_owned()
    }

    async fn events(&self, from: i64, to: i64) -> Vec<CalendarEvent> {
        let ServerResponse::Events { events, .. } = self.calendar.list(from, to).await.unwrap()
        else {
            panic!()
        };
        events
    }

    fn log(&self) -> Vec<String> {
        uplink_storage::open_reader(self.dir.path())
            .unwrap()
            .prepare("SELECT description FROM tx_log ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

fn utc(date_time: &str) -> Value {
    json!({ "dateTime": date_time, "timeZone": "UTC" })
}

/// A weekly meeting, Mondays at 9:00, from 2026-11-02 on.
fn standup() -> Value {
    json!({
        "subject": "Standup",
        "location": { "displayName": "Room 4" },
        "start": utc("2026-11-02T09:00:00"),
        "end": utc("2026-11-02T09:15:00"),
        "recurrence": {
            "pattern": { "type": "weekly", "interval": 1, "daysOfWeek": ["monday"] },
            "range": { "type": "noEnd", "startDate": "2026-11-02" },
        },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn shows_every_occurrence_of_a_series_and_its_exceptions() {
    let setup = Setup::new().await;
    let series = setup.create(standup()).await;
    let instances = setup
        .call(
            reqwest::Method::GET,
            &format!(
                "/events/{series}/instances?startDateTime=2026-11-15T00:00:00Z&endDateTime=2026-11-17T00:00:00Z"
            ),
            None,
        )
        .await;
    let third = instances["value"][0]["id"].as_str().unwrap().to_owned();
    setup
        .call(
            reqwest::Method::PATCH,
            &format!("/events/{third}"),
            Some(json!({
                "subject": "Standup, later",
                "start": utc("2026-11-16T11:00:00"),
                "end": utc("2026-11-16T11:30:00"),
            })),
        )
        .await;
    setup.sync.sync(NOW).await.unwrap();

    // Every Monday for six months, the third one as changed.
    let events = setup.events(NOW, NOW + 182 * DAY).await;
    assert_eq!(events.len(), 26);
    assert!(events.iter().all(|e| e.recurring && e.location == "Room 4"));
    let changed: Vec<_> = events.iter().filter(|e| e.exception).collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].subject, "Standup, later");
    assert_eq!(changed[0].start, NOW + 7 * DAY + 11 * 3_600_000);
    assert!(!events
        .iter()
        .any(|e| e.start == NOW + 7 * DAY + 9 * 3_600_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_changes_and_moves_its_window() {
    let setup = Setup::new().await;
    let dentist = setup
        .create(json!({
            "subject": "Dentist",
            "start": utc("2026-11-10T14:00:00"),
            "end": utc("2026-11-10T15:00:00"),
        }))
        .await;
    setup
        .create(json!({
            "subject": "Holiday",
            "isAllDay": true,
            "start": utc("2026-12-24T00:00:00"),
            "end": utc("2026-12-27T00:00:00"),
        }))
        .await;
    setup.sync.sync(NOW).await.unwrap();
    let subjects = |events: Vec<CalendarEvent>| -> Vec<String> {
        events.into_iter().map(|e| e.subject).collect()
    };
    assert_eq!(
        subjects(setup.events(NOW, NOW + 60 * DAY).await),
        ["Dentist", "Holiday"]
    );
    let holiday = &setup.events(NOW + 40 * DAY, NOW + 60 * DAY).await[0];
    assert!(holiday.all_day);
    assert_eq!(holiday.end - holiday.start, 3 * DAY);

    // Nothing changed: nothing recorded.
    let logged = setup.log().len();
    setup.sync.sync(NOW + 60_000).await.unwrap();
    assert_eq!(setup.log().len(), logged);

    // Changed in Outlook: renamed, then deleted.
    setup
        .call(
            reqwest::Method::PATCH,
            &format!("/events/{dentist}"),
            Some(json!({ "subject": "Dentist (check-up)" })),
        )
        .await;
    setup.sync.sync(NOW + 120_000).await.unwrap();
    assert_eq!(
        subjects(setup.events(NOW, NOW + 2 * DAY).await),
        ["Dentist (check-up)"]
    );
    let response = setup
        .http
        .delete(format!("{}/v1.0/me/events/{dentist}", setup.base))
        .bearer_auth(setup.alice.access_token().await.unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    setup.sync.sync(NOW + 180_000).await.unwrap();
    assert!(setup.events(NOW, NOW + 2 * DAY).await.is_empty());

    // Days on, the window moves: listed anew, further ahead.
    setup.create(standup()).await;
    setup.sync.sync(NOW + 8 * DAY).await.unwrap();
    let log = setup.log();
    assert!(
        log.last().unwrap().starts_with("Calendar listed"),
        "{log:?}"
    );
    let latest = setup.events(NOW + 300 * DAY, NOW + 374 * DAY).await;
    assert_eq!(
        latest.last().unwrap().start,
        NOW + 371 * DAY + 9 * 3_600_000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_anew_once_its_delta_link_expired() {
    let setup = Setup::new().await;
    setup.create(standup()).await;
    setup.sync.sync(NOW).await.unwrap();
    setup
        .create(json!({
            "subject": "Dentist",
            "start": utc("2026-11-10T14:00:00"),
            "end": utc("2026-11-10T15:00:00"),
        }))
        .await;
    // A delta link Graph has forgotten.
    let db = uplink_storage::open(setup.dir.path()).unwrap();
    let state: String = db
        .query_row("SELECT state FROM calendar_sync", [], |r| r.get(0))
        .unwrap();
    let mut state: Value = serde_json::from_str(&state).unwrap();
    let link = state["delta_link"].as_str().unwrap();
    let (at, token) = link.split_once("$deltatoken=").unwrap();
    let mut token: Value = serde_json::from_slice(&base64url(token)).unwrap();
    token["since"] = json!(999);
    state["delta_link"] = json!(format!("{at}$deltatoken={}", unbase64url(&token)));
    db.execute("UPDATE calendar_sync SET state = ?1", [state.to_string()])
        .unwrap();

    setup.sync.sync(NOW + 60_000).await.unwrap();
    let subjects: Vec<String> = setup
        .events(NOW, NOW + 2 * DAY)
        .await
        .into_iter()
        .map(|e| e.subject)
        .collect();
    assert_eq!(subjects, ["Standup", "Dentist"]);
}

fn base64url(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .unwrap()
}

fn unbase64url(v: &Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
}

async fn start_emulate(dir: &Path) -> (String, tokio::process::Child) {
    let emulate = std::env::var_os("UPLINK_EMULATE")
        .expect("UPLINK_EMULATE must name the emulate binary (the dev shell sets it)");
    let config = dir.join("emulate.yaml");
    std::fs::write(
        &config,
        "microsoft:\n  users:\n    - email: alice@example.com\n      name: Alice\n",
    )
    .unwrap();
    // Ports picked and listened on one test at a time, so that no two
    // tests are handed the same.
    static PICKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _picking = PICKING.lock().await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = tokio::process::Command::new(emulate)
        .args([
            "start",
            "--service",
            "microsoft",
            "--port",
            &port.to_string(),
        ])
        .arg("--config")
        .arg(&config)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let ready = format!("{base}/.well-known/openid-configuration");
    for _ in 0..300 {
        if reqwest::get(&ready)
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return (base, child);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("emulate did not start");
}

/// Signs Alice in on emulate's page, as a browser would; returns where
/// Azure AD redirected to.
async fn sign_in(account: &M365AccountConfig) -> String {
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let page = browser
        .get(oauth::authorize_url(account).unwrap())
        .send()
        .await
        .unwrap();
    let page = url::Url::parse(&account.oauth.resource)
        .unwrap()
        .join(page.headers()["location"].to_str().unwrap())
        .unwrap();
    let params: HashMap<String, String> = page.query_pairs().into_owned().collect();
    let mut form: Vec<(&str, &str)> = ["redirect_uri", "scope", "client_id", "response_mode"]
        .iter()
        .map(|k| (*k, params[*k].as_str()))
        .collect();
    form.push(("email", "alice@example.com"));
    let signed_in = browser
        .post(format!(
            "{}/oauth2/v2.0/authorize/callback",
            account.oauth.resource
        ))
        .form(&form)
        .send()
        .await
        .unwrap();
    signed_in.headers()["location"].to_str().unwrap().to_owned()
}
