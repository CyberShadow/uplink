use crate::{service::Calendar, CalendarObserved, ObservedEvent};
use std::sync::Arc;
use uplink_protocol::{ErrorCode, ServerEvent, ServerResponse};
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
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            Registry::new(),
            Arc::new(Hub::new(64)),
        );
        let calendar = Calendar::new(uplink_storage::open_reader(dir.path()).unwrap());
        Self {
            _dir: dir,
            dispatcher,
            calendar,
        }
    }

    async fn observe(&self, replace: bool, events: Vec<ObservedEvent>, removed: &[&str]) -> String {
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
        assert_eq!(
            entry.events,
            [ServerEvent::CalendarChanged(
                uplink_protocol::calendar::CalendarChanged {
                    account: "work".into()
                }
            )]
        );
        entry.description.clone()
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
