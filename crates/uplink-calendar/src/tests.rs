use crate::{service::Calendar, CalendarObserved, ObservedEvent};
use std::sync::Arc;
use uplink_protocol::{
    calendar::{CalendarEvent, EventResponse, ShowAs},
    compose::{OutboxChanged, Pending},
    ErrorCode, ServerEvent, ServerResponse,
};
use uplink_realtime::Hub;
use uplink_tx_log::{Dispatcher, Registry};

const HOUR: i64 = 3_600_000;

struct Db {
    _dir: tempfile::TempDir,
    dispatcher: Dispatcher,
    calendar: Calendar,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        crate::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        let calendar = Calendar::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
            vec![],
        );
        Self {
            _dir: dir,
            dispatcher,
            calendar,
        }
    }

    async fn observe(&self, replace: bool, events: Vec<ObservedEvent>, removed: &[&str]) -> String {
        let (description, events) = self.observing(replace, events, removed).await;
        assert_eq!(
            events,
            [ServerEvent::CalendarChanged(
                uplink_protocol::calendar::CalendarChanged {
                    account: "work".into()
                }
            )]
        );
        description
    }

    /// Observes, returning the entry's description and events.
    async fn observing(
        &self,
        replace: bool,
        events: Vec<ObservedEvent>,
        removed: &[&str],
    ) -> (String, Vec<ServerEvent>) {
        let mut live = self.dispatcher.hub().subscribe();
        self.dispatcher
            .observe(
                "work".to_owned(),
                CalendarObserved {
                    account_kind: "m365",
                    replace,
                    events,
                    removed: removed.iter().map(|r| (*r).to_owned()).collect(),
                    sync_state: serde_json::json!({}),
                },
            )
            .await
            .unwrap();
        let entry = live.recv().await.unwrap();
        (entry.description.clone(), entry.events.clone())
    }

    async fn events(&self) -> Vec<CalendarEvent> {
        let ServerResponse::Events { events, .. } = self.calendar.list(0, 24 * HOUR).await.unwrap()
        else {
            panic!()
        };
        events
    }

    fn pending(&self) -> Vec<(String, EventResponse)> {
        let mut conn = uplink_storage::open_reader(self._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        crate::respond::outbox(&tx)
            .unwrap()
            .into_iter()
            .flat_map(|a| match a.pending {
                Pending::Calendar { responses } => responses,
                other => panic!("{other:?}"),
            })
            .map(|r| (r.subject, r.response))
            .collect()
    }

    async fn subjects(&self, from: i64, to: i64) -> Vec<String> {
        let ServerResponse::Events { events, .. } = self.calendar.list(from, to).await.unwrap()
        else {
            panic!()
        };
        events.into_iter().map(|e| e.subject).collect()
    }
}

fn event(id: &str, subject: &str, start: i64, end: i64) -> ObservedEvent {
    ObservedEvent {
        external_id: id.into(),
        series: None,
        exception: false,
        subject: subject.into(),
        location: String::new(),
        preview: String::new(),
        start,
        end,
        all_day: false,
        cancelled: false,
        show_as: ShowAs::Busy,
        response: EventResponse::Organizer,
        join_url: None,
        reminder: None,
        raw: "{}".into(),
    }
}

#[tokio::test]
async fn records_listings_and_changes() {
    let db = Db::new();
    let description = db
        .observe(
            true,
            vec![
                event("a", "Standup", 9 * HOUR, 10 * HOUR),
                event("b", "Lunch", 12 * HOUR, 13 * HOUR),
                event("c", "Gone soon", 14 * HOUR, 15 * HOUR),
            ],
            &[],
        )
        .await;
    assert_eq!(description, "Calendar listed: 3 events");
    assert_eq!(
        db.subjects(0, 24 * HOUR).await,
        ["Standup", "Lunch", "Gone soon"]
    );
    // Overlapping the range is enough; touching it is not.
    assert_eq!(db.subjects(9 * HOUR + 1, 12 * HOUR).await, ["Standup"]);

    let description = db
        .observe(
            false,
            vec![event("b", "Lunch with Bob", 11 * HOUR, 12 * HOUR)],
            &["c", "never listed"],
        )
        .await;
    assert_eq!(description, "Calendar: 1 event changed, 1 event removed");
    assert_eq!(
        db.subjects(0, 24 * HOUR).await,
        ["Standup", "Lunch with Bob"]
    );

    // A new listing replaces what was there.
    db.observe(true, vec![event("d", "Review", HOUR, 2 * HOUR)], &[])
        .await;
    assert_eq!(db.subjects(0, 24 * HOUR).await, ["Review"]);
}

#[tokio::test]
async fn refuses_ranges_that_are_none_or_too_long() {
    let db = Db::new();
    for (from, to) in [(5, 5), (6, 5), (0, 401 * 24 * HOUR)] {
        assert_eq!(
            db.calendar.list(from, to).await.unwrap_err().code,
            ErrorCode::Invalid
        );
    }
}

#[tokio::test]
async fn queries_match_events_coming_first() {
    let db = Db::new();
    let now = 100 * HOUR;
    let mut standup = event("1", "Standup", now - 50 * HOUR, now - 49 * HOUR);
    standup.location = "Room 4".into();
    db.observe(
        true,
        vec![
            standup,
            event("2", "Standup again", now - 2 * HOUR, now - HOUR),
            event("3", "Standup", now + 2 * HOUR, now + 3 * HOUR),
            event("4", "Lunch", now + HOUR, now + 2 * HOUR),
            event("5", "Ongoing standup", now - HOUR, now + HOUR),
        ],
        &[],
    )
    .await;
    let matching = |query: &str| -> Vec<String> {
        let mut conn = uplink_storage::open_reader(db._dir.path()).unwrap();
        let tx = conn.transaction().unwrap();
        let query = uplink_query::parse(query, now).unwrap();
        crate::matching(&tx, &query, now)
            .unwrap()
            .into_iter()
            .map(|e| format!("{} {}", e.subject, e.start / HOUR))
            .collect()
    };
    assert_eq!(
        matching("standup"),
        [
            "Ongoing standup 99",
            "Standup 102",
            "Standup again 98",
            "Standup 50"
        ]
    );
    assert_eq!(matching("room"), ["Standup 50"], "by location");
    assert_eq!(
        matching("is:event account:work newer_than:1d -standup"),
        ["Lunch 101"]
    );
    assert!(matching("is:todo OR is:unread").is_empty());
}

#[tokio::test]
async fn an_answer_asked_for_prevails_until_synced_and_survives_listings() {
    let db = Db::new();
    let request = |subject: &str, response| ObservedEvent {
        response,
        ..event(subject, subject, 9 * HOUR, 10 * HOUR)
    };
    db.observe(
        true,
        vec![request("Planning", EventResponse::NotResponded)],
        &[],
    )
    .await;
    let id = db.events().await[0].id;
    let ServerResponse::Dispatched { .. } = db
        .calendar
        .respond(id, EventResponse::Accepted)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(db.events().await[0].response, EventResponse::Accepted);

    // Listed anew, not yet as answered: kept, with its id and the answer.
    let outbox_changed = ServerEvent::OutboxChanged(OutboxChanged {
        accounts: vec!["work".into()],
    });
    let (_, events) = db
        .observing(
            true,
            vec![request("Planning", EventResponse::NotResponded)],
            &[],
        )
        .await;
    assert!(!events.contains(&outbox_changed));
    let shown = &db.events().await[0];
    assert_eq!((shown.id, shown.response), (id, EventResponse::Accepted));
    assert_eq!(db.pending(), [("Planning".into(), EventResponse::Accepted)]);

    // Synced as answered: no longer pending.
    let (_, events) = db
        .observing(
            false,
            vec![request("Planning", EventResponse::Accepted)],
            &[],
        )
        .await;
    assert!(events.contains(&outbox_changed));
    assert_eq!(db.pending(), []);
    assert_eq!(db.events().await[0].response, EventResponse::Accepted);

    // Nothing of one's own to answer.
    db.observe(true, vec![event("mine", "Mine", HOUR, 2 * HOUR)], &[])
        .await;
    let mine = db.events().await[0].id;
    assert_eq!(
        db.calendar
            .respond(mine, EventResponse::Declined)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
}
