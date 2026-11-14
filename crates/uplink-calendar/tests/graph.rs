//! Calendar sync against emulate's Microsoft Graph (`UPLINK_EMULATE`, set
//! by the dev shell and `nix flake check`). There is no real account to
//! run it against yet (SPEC §15.12).

use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, process::Stdio, sync::Arc, time::Duration};
use uplink_calendar::{service::Calendar, sync::CalendarSync};
use uplink_config::{M365AccountConfig, M365OAuthConfig, MailServer, Security};
use uplink_email::{
    m365::{api::Graph, oauth},
    oauth::Credentials,
};
use uplink_protocol::{
    calendar::{
        AttendeeKind, CalendarEvent, EventChangeKind, EventField, EventFields, EventResponse,
        Person, Recurrence, RecurrenceEnd, RecurrencePattern, ShowAs, Weekday,
    },
    compose::Pending,
    ServerResponse, TxLogId,
};
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
    dispatcher: Dispatcher,
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
            name: None,
            identities: vec![],
            oauth: M365OAuthConfig {
                client_id: "uplink-test".into(),
                authority: format!("{base}/common"),
                resource: base.clone(),
                imap_resource: base.clone(),
                token_file: dir.path().join("accounts/test.token"),
            },
            // The calendar is not watched by it.
            imap: MailServer {
                host: "localhost".into(),
                port: None,
                security: Security::Tls,
            },
        };
        let http = reqwest::Client::new();
        authorize(&http, &account, &key).await;
        let mut registry = Registry::new();
        uplink_calendar::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        let sync = CalendarSync::new(
            Graph::of(&account, key.clone(), http.clone()),
            account.id.clone(),
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            Duration::from_millis(100),
        );
        Self {
            alice: Credentials::new(http.clone(), oauth::grant(&account), key),
            calendar: Calendar::new(
                dispatcher.clone(),
                uplink_storage::open_reader(dir.path()).unwrap(),
                vec![],
            ),
            dispatcher,
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
    // Changed when the series and its exception were, just now.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(events
        .iter()
        .all(|e| (now - 60_000..=now).contains(&e.modified)));
    let changed: Vec<_> = events.iter().filter(|e| e.exception).collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].subject, "Standup, later");
    assert_eq!(changed[0].start, NOW + 7 * DAY + 11 * 3_600_000);
    assert!(!events
        .iter()
        .any(|e| e.start == NOW + 7 * DAY + 9 * 3_600_000));
}

/// Delta sends a series whole whenever any of it changes, its occurrences
/// with only what is their own, and says no more of an occurrence gone
/// than that it is not there; of a series deleted, it removes the master.
#[tokio::test(flavor = "multi_thread")]
async fn follows_a_series_sent_whole_by_its_master() {
    let setup = Setup::new().await;
    let series = setup.create(standup()).await;
    setup.sync.sync(NOW).await.unwrap();
    let days = |events: Vec<CalendarEvent>| {
        events
            .into_iter()
            .map(|e| (e.start - NOW) / DAY)
            .collect::<Vec<_>>()
    };
    assert_eq!(days(setup.events(NOW, NOW + 21 * DAY).await), [0, 7, 14]);

    // Renamed: the occurrences are as their master is now.
    setup
        .call(
            reqwest::Method::PATCH,
            &format!("/events/{series}"),
            Some(json!({ "subject": "Daily standup" })),
        )
        .await;
    setup.sync.sync(NOW).await.unwrap();
    let events = setup.events(NOW, NOW + 21 * DAY).await;
    assert!(
        events.iter().all(|e| e.subject == "Daily standup"),
        "{events:?}"
    );

    // An occurrence cancelled is left out of the series sent again.
    let instances = setup
        .call(
            reqwest::Method::GET,
            &format!(
                "/events/{series}/instances?startDateTime=2026-11-15T00:00:00Z&endDateTime=2026-11-17T00:00:00Z"
            ),
            None,
        )
        .await;
    let second = instances["value"][0]["id"].as_str().unwrap().to_owned();
    setup
        .call(reqwest::Method::DELETE, &format!("/events/{second}"), None)
        .await;
    setup.sync.sync(NOW).await.unwrap();
    assert_eq!(days(setup.events(NOW, NOW + 21 * DAY).await), [0, 14]);

    // The series deleted: its master removed, and its occurrences with it.
    setup
        .call(reqwest::Method::DELETE, &format!("/events/{series}"), None)
        .await;
    setup.sync.sync(NOW).await.unwrap();
    assert!(setup.events(NOW, NOW + 182 * DAY).await.is_empty());
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

#[tokio::test(flavor = "multi_thread")]
async fn shows_a_meeting_s_status_and_details() {
    let setup = Setup::new().await;
    let id = setup
        .create(json!({
            "subject": "Planning",
            "start": utc("2026-11-10T14:00:00"),
            "end": utc("2026-11-10T15:00:00"),
            "body": {
                "contentType": "html",
                "content": "<p onclick=\"x()\">Agenda: <a href=\"https://example.org/doc\">doc</a><script>x()</script><img src=\"https://img.example.org/a.png\"></p>",
            },
            "organizer": { "emailAddress": { "name": "Bob", "address": "bob@example.com" } },
            "attendees": [
                {
                    "emailAddress": { "name": "Alice", "address": "alice@example.com" },
                    "type": "required",
                    "status": { "response": "none", "time": "0001-01-01T00:00:00Z" },
                },
                {
                    "emailAddress": { "name": "Room 4", "address": "room4@example.com" },
                    "type": "resource",
                    "status": { "response": "accepted", "time": "0001-01-01T00:00:00Z" },
                },
            ],
            "showAs": "tentative",
            "isOnlineMeeting": true,
        }))
        .await;
    setup
        .create(json!({
            "subject": "Canceled: Retro",
            "start": utc("2026-11-11T14:00:00"),
            "end": utc("2026-11-11T15:00:00"),
            "organizer": { "emailAddress": { "name": "Bob", "address": "bob@example.com" } },
            "responseStatus": { "response": "accepted" },
            "isCancelled": true,
            "isReminderOn": false,
        }))
        .await;
    setup.sync.sync(NOW).await.unwrap();

    let events = setup.events(NOW, NOW + 7 * DAY).await;
    // Graph's default reminder, 15 minutes; none, of one turned off.
    assert_eq!(events[0].reminder, Some(15));
    let status: Vec<_> = events
        .iter()
        .map(|e| (e.subject.as_str(), e.cancelled, e.show_as, e.response))
        .collect();
    assert_eq!(
        status,
        [
            (
                "Planning",
                false,
                ShowAs::Tentative,
                EventResponse::NotResponded
            ),
            (
                "Canceled: Retro",
                true,
                ShowAs::Busy,
                EventResponse::Accepted
            ),
        ]
    );
    assert_eq!(events[1].reminder, None);
    let ServerResponse::Event { details, .. } = setup.calendar.get(events[0].id).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(details.event, events[0]);
    assert_eq!(
        details.organizer,
        Some(Person {
            name: "Bob".into(),
            address: Some("bob@example.com".into())
        })
    );
    let attendees: Vec<_> = details
        .attendees
        .iter()
        .map(|a| (a.person.name.as_str(), a.kind, a.response))
        .collect();
    assert_eq!(
        attendees,
        [
            ("Alice", AttendeeKind::Required, EventResponse::None),
            ("Room 4", AttendeeKind::Resource, EventResponse::Accepted),
        ]
    );
    // Sanitized as mail is, its remote images proxied.
    let html = details.body_html.unwrap();
    assert!(
        html.contains(r#"<a href="https://example.org/doc""#),
        "{html}"
    );
    assert!(
        !html.contains("script") && !html.contains("onclick"),
        "{html}"
    );
    assert!(html.contains("/api/remote-image/img.example.org"), "{html}");
    assert_eq!(details.remote_images.len(), 1);
    assert_eq!(details.body_text, None);
    assert!(details
        .event
        .join_url
        .unwrap()
        .ends_with(&format!("/meet/{id}")));
    assert!(details.web_link.unwrap().contains("itemid="));
    assert!(
        details
            .raw
            .as_ref()
            .unwrap()
            .contains("\"isOnlineMeeting\": true"),
        "{:?}",
        details.raw
    );

    let missing = setup
        .calendar
        .get(uplink_protocol::calendar::EventId(999))
        .await;
    assert_eq!(
        missing.unwrap_err().code,
        uplink_protocol::ErrorCode::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_meeting_requests() {
    let setup = Setup::new().await;
    let organizer = json!({ "emailAddress": { "name": "Bob", "address": "bob@example.com" } });
    for (subject, hour) in [("Planning", 9), ("Retro", 10), ("Offsite", 11)] {
        setup
            .create(json!({
                "subject": subject,
                "start": utc(&format!("2026-11-10T{hour:02}:00:00")),
                "end": utc(&format!("2026-11-10T{hour:02}:30:00")),
                "organizer": organizer,
            }))
            .await;
    }
    setup
        .create(json!({
            "subject": "Mine",
            "start": utc("2026-11-10T12:00:00"),
            "end": utc("2026-11-10T13:00:00"),
        }))
        .await;
    setup.sync.sync(NOW).await.unwrap();
    let events = setup.events(NOW, NOW + DAY * 2).await;
    let id = |subject: &str| events.iter().find(|e| e.subject == subject).unwrap().id;
    let answers = || async {
        setup
            .events(NOW, NOW + DAY * 2)
            .await
            .into_iter()
            .map(|e| (e.subject, e.response))
            .collect::<Vec<_>>()
    };
    let setup = &setup;
    let respond = |subject: &str, response| {
        let event = id(subject);
        async move {
            match setup.calendar.respond(event, response).await {
                Ok(ServerResponse::Dispatched { tx_log_id }) => tx_log_id,
                other => panic!("{other:?}"),
            }
        }
    };
    let outbox = || {
        let mut conn = uplink_storage::open_reader(setup.dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        uplink_calendar::outbox(&tx)
            .unwrap()
            .into_iter()
            .flat_map(|a| match a.pending {
                Pending::Calendar { responses, .. } => responses,
                other => panic!("{other:?}"),
            })
            .map(|r| (r.subject, r.response, r.refused))
            .collect::<Vec<_>>()
    };

    // Shown at once (one declined, as not listed); given as the sync task
    // pushes.
    let accepted = respond("Planning", EventResponse::Accepted).await;
    respond("Retro", EventResponse::TentativelyAccepted).await;
    respond("Offsite", EventResponse::Declined).await;
    let answered = [
        ("Planning".into(), EventResponse::Accepted),
        ("Retro".into(), EventResponse::TentativelyAccepted),
        ("Mine".into(), EventResponse::Organizer),
    ];
    assert_eq!(answers().await, answered);
    assert_eq!(outbox().len(), 3);
    setup.sync.push().await.unwrap();
    assert_eq!(outbox(), []);
    let log = setup.log();
    assert_eq!(
        log[log.len() - 3..],
        [
            "Accepted 'Planning' at the provider",
            "Tentatively accepted 'Retro' at the provider",
            "Declined 'Offsite' at the provider",
        ]
    );
    // Declined, it leaves the calendar, as Exchange moves it to Deleted
    // Items; the others are as answered.
    setup.sync.sync(NOW + 60_000).await.unwrap();
    assert_eq!(answers().await, answered);
    assert_eq!(
        setup.calendar.get(id("Offsite")).await.unwrap_err().code,
        uplink_protocol::ErrorCode::NotFound
    );

    // An answer given cannot be taken back: undone, it needs a decision,
    // which undoing the undo makes.
    let undo = setup.dispatcher.undo(accepted).await.unwrap();
    assert_eq!(answers().await[0].1, EventResponse::NotResponded);
    setup.sync.push().await.unwrap();
    assert_eq!(
        outbox(),
        [(
            "Planning".into(),
            EventResponse::NotResponded,
            Some(uplink_calendar::respond::CANNOT_TAKE_BACK.into())
        )]
    );
    setup.dispatcher.undo(undo).await.unwrap();
    assert_eq!(outbox(), []);
    assert_eq!(answers().await[0].1, EventResponse::Accepted);

    // Nothing to answer of one's own, nor answers but these.
    for (subject, response) in [
        ("Mine", EventResponse::Accepted),
        ("Planning", EventResponse::NotResponded),
    ] {
        let refused = setup.calendar.respond(id(subject), response).await;
        assert_eq!(
            refused.unwrap_err().code,
            uplink_protocol::ErrorCode::Invalid
        );
    }
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

/// Signs `user` in on emulate's page, where the authorization `url`
/// leads, as a browser would; returns where Entra ID redirected to.
async fn sign_in(base: &str, url: url::Url, user: &str) -> String {
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let page = browser.get(url).send().await.unwrap();
    assert_eq!(page.status(), 302);
    let page = url::Url::parse(base)
        .unwrap()
        .join(page.headers()["location"].to_str().unwrap())
        .unwrap();
    let params: HashMap<String, String> = page.query_pairs().into_owned().collect();
    let mut form: Vec<(&str, &str)> = [
        "redirect_uri",
        "scope",
        "client_id",
        "state",
        "code_challenge",
        "code_challenge_method",
    ]
    .iter()
    .filter_map(|k| Some((*k, params.get(*k)?.as_str())))
    .collect();
    form.push(("email", user));
    let signed_in = browser
        .post(format!("{base}/oauth2/v2.0/authorize/callback"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), 302);
    let location = signed_in.headers()["location"].to_str().unwrap().to_owned();
    assert!(location.starts_with(&params["redirect_uri"]), "{location}");
    location
}

/// Runs `uplink oauth init`'s flow for `account`, signing Alice in as a
/// browser would: the redirect followed back to the flow's listener.
async fn authorize(http: &reqwest::Client, account: &M365AccountConfig, key: &MasterKey) {
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let (authorized, ()) = tokio::join!(
        oauth::authorize(
            account,
            key,
            |url| url_tx.send(url.clone()).unwrap(),
            std::future::pending(),
        ),
        async {
            let location = sign_in(
                &account.oauth.resource,
                url_rx.await.unwrap(),
                &account.address,
            )
            .await;
            assert!(http
                .get(location)
                .send()
                .await
                .unwrap()
                .status()
                .is_success());
        },
    );
    authorized.unwrap();
}

/// What the calendar's outbox has of event writes: kind, subject, series.
fn writes(setup: &Setup) -> Vec<(EventChangeKind, String, bool)> {
    let mut conn = uplink_storage::open_reader(setup.dir.path()).unwrap();
    let tx = conn.transaction().unwrap();
    uplink_calendar::outbox(&tx)
        .unwrap()
        .into_iter()
        .flat_map(|a| match a.pending {
            Pending::Calendar { changes, .. } => changes,
            other => panic!("{other:?}"),
        })
        .map(|c| (c.change, c.subject, c.series))
        .collect()
}

fn fields(subject: &str, start: i64, end: i64) -> EventFields {
    EventFields {
        subject: subject.into(),
        location: String::new(),
        body: String::new(),
        start,
        end,
        all_day: false,
        required: String::new(),
        optional: String::new(),
        online_meeting: false,
        reminder: Some(15),
        show_as: ShowAs::Busy,
        recurrence: None,
    }
}

fn dispatched(response: uplink_tx_log::answer::Response) -> TxLogId {
    match response {
        Ok(ServerResponse::Dispatched { tx_log_id }) => tx_log_id,
        other => panic!("{other:?}"),
    }
}

/// Tuesday 2026-11-10 10:00 in Prague (CET): 09:00 UTC.
const REVIEW: i64 = 1_794_301_200_000;
const HOUR: i64 = 3_600_000;

#[tokio::test(flavor = "multi_thread")]
async fn creates_changes_and_deletes_events() {
    let setup = Setup::new().await;
    setup.sync.sync(NOW).await.unwrap();
    let prague = "Europe/Prague".to_owned();
    let subjects = || async {
        setup
            .events(NOW, NOW + 200 * DAY)
            .await
            .into_iter()
            .map(|e| (e.subject, e.start))
            .collect::<Vec<_>>()
    };

    // A meeting: shown at once, created as the sync pushes, in Prague's
    // time, its attendee invited.
    let review = EventFields {
        location: "Room 4".into(),
        body: "<p>The <b>draft</b></p>".into(),
        required: "Bob <bob@example.com>".into(),
        online_meeting: true,
        ..fields("Review", REVIEW, REVIEW + HOUR)
    };
    dispatched(
        setup
            .calendar
            .create("test".into(), review.clone(), prague.clone())
            .await,
    );
    let shown = setup.events(NOW, NOW + DAY * 2).await;
    assert_eq!(
        (shown[0].subject.as_str(), shown[0].start, shown[0].response),
        ("Review", REVIEW, EventResponse::Organizer)
    );
    let id = shown[0].id;
    assert_eq!(
        writes(&setup),
        [(EventChangeKind::Create, "Review".into(), false)]
    );
    assert!(setup.sync.push().await.unwrap());
    assert_eq!(writes(&setup), []);
    let at_graph = setup
        .call(
            reqwest::Method::GET,
            "/calendarView?startDateTime=2026-11-10T00:00:00Z&endDateTime=2026-11-11T00:00:00Z",
            None,
        )
        .await;
    let created = &at_graph["value"][0];
    assert_eq!(created["subject"], "Review");
    assert_eq!(created["start"]["dateTime"], "2026-11-10T09:00:00.0000000");
    assert_eq!(
        created["originalStartTimeZone"],
        "Central Europe Standard Time"
    );
    assert_eq!(
        created["attendees"][0]["emailAddress"]["address"],
        "bob@example.com"
    );
    assert_eq!(created["body"]["content"], "<p>The <b>draft</b></p>");
    assert!(created["onlineMeeting"]["joinUrl"].is_string());
    setup.sync.sync(NOW + 1).await.unwrap();
    let synced = setup.events(NOW, NOW + DAY * 2).await;
    assert_eq!((synced.len(), synced[0].id), (1, id));
    assert!(synced[0].join_url.is_some());
    let ServerResponse::Event { details, .. } = setup.calendar.get(id).await.unwrap() else {
        panic!()
    };
    let editable = details.editable.unwrap();
    assert_eq!(
        (editable.required.as_str(), editable.location.as_str()),
        ("\"Bob\" <bob@example.com>", "Room 4")
    );

    // An edit undone before it is given gives nothing.
    let moved = EventFields {
        subject: "Review, moved".into(),
        ..fields("", REVIEW + HOUR, REVIEW + 2 * HOUR)
    };
    let edit = dispatched(
        setup
            .calendar
            .update(
                id,
                false,
                moved.clone(),
                vec![EventField::Subject, EventField::Time],
                prague.clone(),
            )
            .await,
    );
    assert_eq!(subjects().await, [("Review, moved".into(), REVIEW + HOUR)]);
    assert_eq!(
        writes(&setup),
        [(EventChangeKind::Update, "Review, moved".into(), false)]
    );
    setup.dispatcher.undo(edit).await.unwrap();
    assert_eq!(subjects().await, [("Review".into(), REVIEW)]);
    assert_eq!(writes(&setup), []);
    // Given: only what was edited.
    dispatched(
        setup
            .calendar
            .update(
                id,
                false,
                moved,
                vec![EventField::Subject, EventField::Time],
                prague.clone(),
            )
            .await,
    );
    setup.sync.push().await.unwrap();
    assert_eq!(writes(&setup), []);
    let at_graph = setup
        .call(
            reqwest::Method::GET,
            &format!("/events/{}", created["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(
        (
            &at_graph["subject"],
            &at_graph["start"]["dateTime"],
            &at_graph["location"]["displayName"]
        ),
        (
            &json!("Review, moved"),
            &json!("2026-11-10T10:00:00.0000000"),
            &json!("Room 4")
        )
    );
    assert_eq!(at_graph["attendees"].as_array().unwrap().len(), 1);

    // A series, Mondays at 10:00 in Prague, across the change to summer
    // time: its first occurrence shown at once, all once given.
    let first = 1_805_706_000_000; // 2027-03-22 09:00 UTC
    let weekly = EventFields {
        recurrence: Some(Recurrence {
            pattern: RecurrencePattern::Weekly {
                days: vec![Weekday::Monday],
            },
            interval: 1,
            until: RecurrenceEnd::After { count: 3 },
        }),
        ..fields("Weekly", first, first + HOUR / 2)
    };
    dispatched(
        setup
            .calendar
            .create("test".into(), weekly, prague.clone())
            .await,
    );
    assert_eq!(
        subjects().await,
        [
            ("Review, moved".into(), REVIEW + HOUR),
            ("Weekly".into(), first)
        ]
    );
    assert_eq!(
        writes(&setup),
        [(EventChangeKind::Create, "Weekly".into(), true)]
    );
    setup.sync.step(NOW + 2).await.unwrap();
    let weeks = [first, 1_806_307_200_000, 1_806_912_000_000];
    assert_eq!(
        subjects().await,
        [
            ("Review, moved".into(), REVIEW + HOUR),
            ("Weekly".into(), weeks[0]),
            ("Weekly".into(), weeks[1]),
            ("Weekly".into(), weeks[2]),
        ]
    );
    assert_eq!(writes(&setup), []);

    // The series changed: shown on its occurrences at once.
    let occurrences = setup.events(first - DAY, first + 20 * DAY).await;
    let ServerResponse::Event { details, .. } =
        setup.calendar.get(occurrences[1].id).await.unwrap()
    else {
        panic!()
    };
    let series = details.series.expect("the series' fields");
    assert_eq!(series.start, first);
    let renamed = EventFields {
        subject: "Weekly sync".into(),
        ..series
    };
    dispatched(
        setup
            .calendar
            .update(
                occurrences[1].id,
                true,
                renamed,
                vec![EventField::Subject],
                prague.clone(),
            )
            .await,
    );
    assert_eq!(
        setup
            .events(first - DAY, first + 20 * DAY)
            .await
            .iter()
            .map(|e| e.subject.as_str())
            .collect::<Vec<_>>(),
        ["Weekly sync"; 3]
    );
    assert_eq!(
        writes(&setup),
        [(EventChangeKind::Update, "Weekly sync".into(), true)]
    );
    setup.sync.step(NOW + 3).await.unwrap();
    assert_eq!(writes(&setup), []);
    assert_eq!(setup.events(first - DAY, first + 20 * DAY).await.len(), 3);

    // One occurrence deleted: off at once, and at the provider.
    let occurrences = setup.events(first - DAY, first + 20 * DAY).await;
    dispatched(setup.calendar.delete(occurrences[1].id, false).await);
    assert_eq!(setup.events(first - DAY, first + 20 * DAY).await.len(), 2);
    setup.sync.step(NOW + 4).await.unwrap();
    let starts: Vec<i64> = setup
        .events(first - DAY, first + 20 * DAY)
        .await
        .iter()
        .map(|e| e.start)
        .collect();
    assert_eq!(starts, [weeks[0], weeks[2]]);

    // The meeting deleted is cancelled, its attendee told; undone, it is
    // created anew.
    let deleted = dispatched(setup.calendar.delete(id, false).await);
    assert_eq!(setup.events(NOW, NOW + DAY * 2).await, []);
    setup.sync.step(NOW + 5).await.unwrap();
    assert!(setup
        .log()
        .contains(&"Cancelled 'Review, moved'".to_owned()));
    let gone = setup
        .call(
            reqwest::Method::GET,
            "/calendarView?startDateTime=2026-11-10T00:00:00Z&endDateTime=2026-11-11T00:00:00Z",
            None,
        )
        .await;
    assert_eq!(gone["value"], json!([]));
    setup.dispatcher.undo(deleted).await.unwrap();
    assert_eq!(
        setup
            .events(NOW, NOW + DAY * 2)
            .await
            .iter()
            .map(|e| e.subject.as_str())
            .collect::<Vec<_>>(),
        ["Review, moved"]
    );
    setup.sync.step(NOW + 6).await.unwrap();
    let again = setup
        .call(
            reqwest::Method::GET,
            "/calendarView?startDateTime=2026-11-10T00:00:00Z&endDateTime=2026-11-11T00:00:00Z",
            None,
        )
        .await;
    assert_eq!(again["value"][0]["subject"], "Review, moved");
    assert_eq!(
        again["value"][0]["attendees"][0]["emailAddress"]["address"],
        "bob@example.com"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_created_though_its_outcome_was_unknown_is_found_by_its_transaction_id() {
    let setup = Setup::new().await;
    setup.sync.sync(NOW).await.unwrap();
    dispatched(
        setup
            .calendar
            .create(
                "test".into(),
                fields("Once", REVIEW, REVIEW + HOUR),
                "UTC".into(),
            )
            .await,
    );
    // As if a push had created it, its answer lost.
    let transaction_id: String = uplink_storage::open_reader(setup.dir.path())
        .unwrap()
        .query_row("SELECT transaction_id FROM calendar_event", [], |r| {
            r.get(0)
        })
        .unwrap();
    setup
        .create(json!({
            "subject": "Once",
            "start": utc("2026-11-10T09:00:00"),
            "end": utc("2026-11-10T10:00:00"),
            "transactionId": transaction_id,
        }))
        .await;
    setup.sync.sync(NOW + 1).await.unwrap();
    assert_eq!(writes(&setup), []);
    assert_eq!(setup.events(NOW, NOW + DAY * 2).await.len(), 1);
    assert!(!setup.sync.push().await.unwrap());
    let at_graph = setup
        .call(
            reqwest::Method::GET,
            "/calendarView?startDateTime=2026-11-10T00:00:00Z&endDateTime=2026-11-11T00:00:00Z",
            None,
        )
        .await;
    assert_eq!(at_graph["value"].as_array().unwrap().len(), 1);
}
