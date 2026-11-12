//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position. Each
//! domain matches its own; a term a kind lacks is false of it. The home
//! view (SPEC §12.13) is the matches of its buckets' queries.

use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uplink_config::HomeBucket;
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_matrix::query::Rooms;
use uplink_protocol::{
    calendar::CalendarEvent, mail::ListPosition, matrix::Room, todos::Todo, ErrorCode, ServerError,
    ServerResponse,
};
use uplink_query::{Query, Term};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most conversations a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

/// The most rooms' messages a search lists: the latest; a room's own
/// search finds more.
const MESSAGES: u32 = 50;

#[derive(Clone)]
pub struct Items {
    reader: Reader,
    mail_accounts: Vec<mail::ConfiguredAccount>,
    matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
    /// As configured, top to bottom; configured anew on reload.
    home: Arc<ArcSwap<Vec<HomeBucket>>>,
}

/// What a query matches of kinds other than mail.
struct Others {
    rooms: Vec<Room>,
    todos: Vec<Todo>,
    events: Vec<CalendarEvent>,
}

impl Items {
    /// `reader` is a read-only connection to the database.
    pub fn new(
        reader: Connection,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
        home: Vec<HomeBucket>,
    ) -> Self {
        Self {
            reader: Reader::new(reader),
            mail_accounts,
            matrix_accounts,
            home: Arc::new(ArcSwap::from_pointee(home)),
        }
    }

    pub fn configure_home(&self, home: Vec<HomeBucket>) {
        self.home.store(Arc::new(home));
    }

    pub async fn list(
        &self,
        query: String,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Response {
        let (today, now) = checked(limit)?;
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let page = this.mail(&tx, &query, after, limit)?;
                // A page after `after` is of mail alone.
                let (others, messages) = match after {
                    None => {
                        let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                        let messages = rooms.messages(&tx, &query, MESSAGES).map_err(internal)?;
                        (others(&tx, &rooms, &query, today, now)?, messages)
                    }
                    Some(_) => (
                        Others {
                            rooms: vec![],
                            todos: vec![],
                            events: vec![],
                        },
                        vec![],
                    ),
                };
                Ok(ServerResponse::Items {
                    as_of,
                    conversations: page.conversations,
                    next: page.next,
                    scope: page.scope,
                    rooms: others.rooms,
                    messages,
                    todos: others.todos,
                    events: others.events,
                })
            })
            .await
    }

    /// Each bucket lists what its query matches, less what those before it
    /// list. A name no kind has is false, not an error: the default
    /// buckets name Gmail's labels and Element's tags, which an account
    /// may lack.
    pub async fn home(&self, limit: u32) -> Response {
        let (today, now) = checked(limit)?;
        let mut before: Vec<&str> = vec![];
        let mut buckets = vec![];
        let configured = self.home.load_full();
        for bucket in configured.iter() {
            let text = std::iter::once(format!("({})", bucket.query))
                .chain(before.iter().map(|q| format!("-({q})")))
                .collect::<Vec<_>>()
                .join(" ");
            let query = uplink_query::parse(&text, now)
                .expect("buckets' queries are checked as configured, so parse in parentheses");
            before.push(&bucket.query);
            buckets.push((bucket.name.clone(), text, query));
        }
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                // Read once, for every bucket.
                let rooms = Rooms::load(&tx, &this.matrix_accounts).map_err(internal)?;
                let buckets = buckets
                    .into_iter()
                    .map(|(name, text, query)| {
                        let page = this.mail(&tx, &query, None, limit)?;
                        let others = others(&tx, &rooms, &query, today, now)?;
                        Ok(uplink_protocol::HomeBucket {
                            name,
                            query: text,
                            conversations: page.conversations,
                            more: page.next.is_some(),
                            rooms: others.rooms,
                            todos: others.todos,
                            events: others.events,
                        })
                    })
                    .collect::<Result<_, ServerError>>()?;
                Ok(ServerResponse::Home { as_of, buckets })
            })
            .await
    }

    fn mail(
        &self,
        tx: &Transaction,
        query: &Query,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Result<mail::Page, ServerError> {
        mail::conversations(tx, &self.mail_accounts, query, Unknown::False, after, limit).map_err(
            |e| match e {
                ListError::Invalid(e) => invalid(e.0),
                ListError::Internal(e) => internal(e),
            },
        )
    }

    /// Fails on a label or account no kind has: a misspelt name lists
    /// nothing, as in a list of mail.
    fn check_names(&self, tx: &Transaction, query: &Query) -> Result<(), ServerError> {
        for term in uplink_query::terms(query) {
            match term {
                Term::Label(name) => {
                    let known = mail::label_known(tx, name).map_err(internal)?
                        || uplink_matrix::query::tag_known(tx, name).map_err(internal)?;
                    if !known {
                        return Err(invalid(format!("no label or tag {name}")));
                    }
                }
                Term::Account(account) => {
                    let known = self
                        .mail_accounts
                        .iter()
                        .any(|a| a.id == *account || a.address.eq_ignore_ascii_case(account))
                        || self
                            .matrix_accounts
                            .iter()
                            .any(|a| a.id == *account || a.mxid == *account);
                    if !known {
                        return Err(invalid(format!("no account {account}")));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn others(
    tx: &Transaction,
    rooms: &Rooms,
    query: &Query,
    today: i64,
    now: i64,
) -> Result<Others, ServerError> {
    Ok(Others {
        rooms: rooms.matching(query),
        todos: uplink_todos::matching(tx, query, today).map_err(internal)?,
        events: uplink_calendar::matching(tx, query, now).map_err(internal)?,
    })
}

fn invalid(why: String) -> ServerError {
    error(ErrorCode::Invalid, why)
}

/// Today, as days since 1970-01-01, and now, in ms, for a `limit` checked.
fn checked(limit: u32) -> Result<(i64, i64), ServerError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64;
    Ok((uplink_query::today(now), now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uplink_matrix::rooms::{ObservedRoom, Synced};
    use uplink_protocol::matrix::Membership;
    use uplink_realtime::Hub;
    use uplink_tx_log::{Dispatcher, Registry};

    #[tokio::test]
    async fn queries_list_every_kind_and_names_must_be_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::new();
        uplink_matrix::register(&mut registry);
        uplink_todos::register(&mut registry);
        let dispatcher = Dispatcher::new(
            uplink_storage::open(dir.path()).unwrap(),
            registry,
            Arc::new(Hub::new(64)),
        );
        dispatcher
            .observe(
                "mx".into(),
                Synced {
                    next_batch: "next".into(),
                    rooms: vec![ObservedRoom {
                        room_id: "!r:x".into(),
                        name: "Rent talk".into(),
                        encrypted: false,
                        membership: Membership::Invited,
                        space: false,
                        direct: false,
                        avatar: None,
                        children: vec![],
                        space_order: None,
                        tags: vec!["u.home".into()],
                        topic: None,
                        alias: None,
                        pinned: vec![],
                        may_pin: false,
                        joined_members: 1,
                        version: "11".into(),
                        events: vec![],
                        limited: false,
                        own_receipts: vec![],
                    }],
                },
            )
            .await
            .unwrap();
        let todos = uplink_todos::service::Todos::new(
            dispatcher.clone(),
            uplink_storage::open_reader(dir.path()).unwrap(),
        );
        todos
            .create(
                "Pay rent".into(),
                String::new(),
                Some("2026-11-10".into()),
                None,
            )
            .await
            .unwrap();
        let items = Items::new(
            uplink_storage::open_reader(dir.path()).unwrap(),
            vec![],
            vec![uplink_matrix::service::ConfiguredAccount {
                id: "mx".into(),
                mxid: "@me:x".into(),
            }],
            vec![],
        );
        let list = |query: &str| {
            let items = items.clone();
            let query = query.to_owned();
            async move { items.list(query, None, 10).await }
        };
        let names = |response: Response| match response.unwrap() {
            ServerResponse::Items {
                conversations,
                rooms,
                todos,
                events,
                ..
            } => {
                assert!(conversations.is_empty() && events.is_empty());
                let rooms = rooms.into_iter().map(|r| r.name);
                rooms
                    .chain(todos.into_iter().map(|t| t.title))
                    .collect::<Vec<_>>()
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(names(list("rent").await), ["Rent talk", "Pay rent"]);
        assert_eq!(
            names(list("is:invite OR is:overdue").await),
            ["Rent talk", "Pay rent"]
        );
        assert_eq!(names(list("tag:home").await), ["Rent talk"]);
        assert_eq!(names(list("rent -account:mx").await), ["Pay rent"]);
        let refused = |response: Response| response.unwrap_err().message;
        assert_eq!(refused(list("tag:work").await), "no label or tag work");
        assert_eq!(refused(list("account:nobody").await), "no account nobody");
        assert!(refused(items.list("x".into(), None, 0).await).starts_with("limit"));

        // An item is in the first bucket that matches it; a name no kind
        // has is false.
        let bucket = |name: &str, query: &str| HomeBucket {
            name: name.into(),
            query: query.into(),
        };
        items.configure_home(vec![
            bucket("Now", "is:invite"),
            bucket("Rent", "rent"),
            bucket("Else", "tag:nowhere OR is:todo"),
        ]);
        let ServerResponse::Home { buckets, .. } =
            items.home(10).await.unwrap()
        else {
            panic!()
        };
        let listed: Vec<_> = buckets
            .iter()
            .map(|b| {
                let rooms = b.rooms.iter().map(|r| r.name.clone());
                let todos = b.todos.iter().map(|t| t.title.clone());
                (b.name.as_str(), rooms.chain(todos).collect::<Vec<_>>())
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("Now", vec!["Rent talk".to_owned()]),
                ("Rent", vec!["Pay rent".to_owned()]),
                ("Else", vec![]),
            ]
        );
        assert_eq!(
            buckets[2].query,
            "(tag:nowhere OR is:todo) -(is:invite) -(rent)"
        );
        assert!(!buckets[0].more);
    }
}
