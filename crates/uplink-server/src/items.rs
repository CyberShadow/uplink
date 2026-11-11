//! What a query matches of every kind (SPEC §8.7): mail's conversations,
//! rooms, to-dos and events, in one read, so as of one log position. Each
//! domain matches its own; a term a kind lacks is false of it.

use std::time::{SystemTime, UNIX_EPOCH};
use uplink_email::query::{self as mail, ListError, Unknown};
use uplink_protocol::{mail::ListPosition, ErrorCode, ServerResponse};
use uplink_query::{Query, Term};
use uplink_storage::rusqlite::{Connection, Transaction};
use uplink_tx_log::answer::{error, internal, Reader, Response};

/// The most conversations a page may ask for, as a list of mail.
const MAX_LIMIT: u32 = uplink_email::service::MAX_LIMIT;

#[derive(Clone)]
pub struct Items {
    reader: Reader,
    mail_accounts: Vec<mail::ConfiguredAccount>,
    matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
}

impl Items {
    /// `reader` is a read-only connection to the database.
    pub fn new(
        reader: Connection,
        mail_accounts: Vec<mail::ConfiguredAccount>,
        matrix_accounts: Vec<uplink_matrix::service::ConfiguredAccount>,
    ) -> Self {
        Self {
            reader: Reader::new(reader),
            mail_accounts,
            matrix_accounts,
        }
    }

    pub async fn list(
        &self,
        query: String,
        today: String,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Response {
        let invalid = |why: String| error(ErrorCode::Invalid, why);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")));
        }
        let today = uplink_query::days(&today)
            .ok_or_else(|| invalid(format!("today {today:?} is not a date, YYYY-MM-DD")))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;
        let query = uplink_query::parse(&query, now).map_err(|e| invalid(e.0))?;
        let this = self.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                this.check_names(&tx, &query)?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let page = mail::conversations(
                    &tx,
                    &this.mail_accounts,
                    &query,
                    Unknown::False,
                    after,
                    limit,
                )
                .map_err(|e| match e {
                    ListError::Invalid(e) => invalid(e.0),
                    ListError::Internal(e) => internal(e),
                })?;
                let first = after.is_none();
                let (rooms, todos, events) = if first {
                    (
                        uplink_matrix::query::rooms(&tx, &this.matrix_accounts, &query)
                            .map_err(internal)?,
                        uplink_todos::matching(&tx, &query, today).map_err(internal)?,
                        uplink_calendar::matching(&tx, &query, now).map_err(internal)?,
                    )
                } else {
                    (vec![], vec![], vec![])
                };
                Ok(ServerResponse::Items {
                    as_of,
                    conversations: page.conversations,
                    next: page.next,
                    scope: page.scope,
                    rooms,
                    todos,
                    events,
                })
            })
            .await
    }

    /// Fails on a label or account no kind has: a misspelt name lists
    /// nothing, as in a list of mail.
    fn check_names(
        &self,
        tx: &Transaction,
        query: &Query,
    ) -> Result<(), uplink_protocol::ServerError> {
        for term in uplink_query::terms(query) {
            match term {
                Term::Label(name) => {
                    let known = mail::label_known(tx, name).map_err(internal)?
                        || uplink_matrix::query::tag_known(tx, name).map_err(internal)?;
                    if !known {
                        return Err(error(ErrorCode::Invalid, format!("no label or tag {name}")));
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
                        return Err(error(ErrorCode::Invalid, format!("no account {account}")));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
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
        );
        let list = |query: &str| {
            let items = items.clone();
            let query = query.to_owned();
            async move { items.list(query, "2026-11-11".into(), None, 10).await }
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
        assert!(
            refused(items.list("x".into(), "today".into(), None, 10).await).starts_with("today")
        );
        assert!(
            refused(items.list("x".into(), "2026-11-11".into(), None, 0).await)
                .starts_with("limit")
        );
    }
}
