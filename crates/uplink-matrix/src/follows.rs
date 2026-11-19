//! Following threads (MSC4306 thread subscriptions, SPEC §9.2, §9.5), as
//! Slack's: the replies in a thread one follows notify, those in one not
//! followed do not, but for mentions. The homeserver keeps whether the
//! account is subscribed to a thread, and its push rules say what that
//! does (`.io.element.msc4306.rule.{unsubscribed,subscribed}_thread`).
//!
//! Following is desired state (§6.6), a field of each thread, by its root
//! (§6.6.4, a set): following or unfollowing records the follow asked
//! for, and the account's sync task subscribes, or unsubscribes, on the
//! homeserver, and reads it back. Sending in a thread follows it, as does
//! sending a message, as the root of a thread others may start by
//! replying to it; a mention in a thread not followed follows it, as the
//! homeserver allows (an automatic subscription, which it skips if the
//! user unfollowed since).
//!
//! The push rules are evaluated by the client, and the SDK's evaluation
//! does not know the threads followed: those of messages of threads are
//! evaluated again here ([`Evaluator`]), each thread as uplink has it.

use anyhow::{Context, Result};
use matrix_sdk::{
    deserialized_responses::TimelineEvent,
    room::PushContext,
    ruma::{
        api::{
            client::threads::{get_thread_subscription, subscribe_thread, unsubscribe_thread},
            error::ErrorKind,
            FeatureFlag,
        },
        events::room::member::MembershipState,
        EventId, OwnedEventId, UserId,
    },
    Client, Room,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use uplink_protocol::{
    matrix::{FollowChange, RoomEventId, RoomId, RoomsChanged, TimelineChanged},
    ServerEvent, TxLogId,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{
    reconcile::{Ended, Field},
    this_entry, Applied, Conflict, Inverse, Observation, Observed, Op,
};

use crate::{
    outbox,
    reconciler::{self, Homeserver, Kind, Requests, RoomIdRef, Stored},
    rooms::{self, ObservedEvent, Push},
    sync::read,
};

/// Whether the account follows a thread (SPEC §6.6): subscribed to it
/// (`observed`), or asked to be (`desired`).
pub type Follow = Field<bool>;

/// The thread of `root` as stored, if it has a row; else as known of
/// every thread it has none of: not followed, nor asked to be, since
/// ever.
fn load(conn: &Connection, room: RoomId, root: &str) -> Result<Follow> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT field FROM matrix_thread_follow WHERE room = ?1 AND root = ?2",
            params![room.0, root],
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        Some(json) => parse_field(&json),
        None => Ok(Field::new(false, TxLogId(0))),
    }
}

fn parse_field(json: &str) -> Result<Follow> {
    serde_json::from_str(json).with_context(|| format!("a thread's follow: {json}"))
}

fn store(tx: &Transaction, room: RoomId, root: &str, field: &Follow) -> Result<()> {
    tx.execute(
        "INSERT INTO matrix_thread_follow (room, root, field) VALUES (?1, ?2, ?3)
         ON CONFLICT (room, root) DO UPDATE SET field = excluded.field",
        params![room.0, root, serde_json::to_string(field)?],
    )?;
    Ok(())
}

/// Of an event `e` (SQL), whether it is of a thread the account follows,
/// as shown: `is:followed`.
pub(crate) const FOLLOWED: &str = "(e.rel_type = 'm.thread' AND EXISTS (
      SELECT 1 FROM matrix_thread_follow f
      WHERE f.room = e.room AND f.root = e.relates_to AND f.desired))";

/// What changed for clients as the thread of `root` came to be shown
/// followed or not (`followed`): its root, as listed; and once not, its
/// messages stored no longer notify, but those that highlight, so what
/// stood of them is let go (SPEC §12.16), as their push rules would
/// have had it.
fn shown(
    tx: &Transaction,
    room: RoomId,
    root: &str,
    followed: bool,
    events: &mut Changes,
) -> Result<()> {
    let mut changed: Vec<RoomEventId> = tx
        .prepare("SELECT id FROM matrix_event WHERE room = ?1 AND event_id = ?2")?
        .query_map(params![room.0, root], |r| Ok(RoomEventId(r.get(0)?)))?
        .collect::<Result<_, _>>()?;
    if !followed {
        let quiet: Vec<RoomEventId> = tx
            .prepare(
                "UPDATE matrix_event SET notify = 0
                 WHERE room = ?1 AND rel_type = 'm.thread' AND relates_to = ?2
                   AND notify AND NOT highlight
                 RETURNING id",
            )?
            .query_map(params![room.0, root], |r| Ok(RoomEventId(r.get(0)?)))?
            .collect::<Result<_, _>>()?;
        changed.extend(quiet);
    }
    events.rooms.insert(room);
    events.events.entry(room).or_default().extend(changed);
    Ok(())
}

/// What changed for clients: rooms, and their events.
#[derive(Default)]
struct Changes {
    rooms: BTreeSet<RoomId>,
    events: BTreeMap<RoomId, BTreeSet<RoomEventId>>,
}

impl Changes {
    fn into_events(self) -> Vec<ServerEvent> {
        let mut events = vec![];
        if !self.rooms.is_empty() {
            events.push(ServerEvent::RoomsChanged(RoomsChanged {
                rooms: self.rooms.into_iter().collect(),
            }));
        }
        for (room, changed) in self.events {
            if !changed.is_empty() {
                events.push(ServerEvent::TimelineChanged(TimelineChanged {
                    room,
                    events: changed.into_iter().collect(),
                }));
            }
        }
        events
    }
}

/// Records the room's thread of `root` as followed or not, as observed
/// by a request sent after `as_of` (SPEC §6.6.2); returns whether that
/// shown changed, telling `events` of what changed.
fn observe_into(
    tx: &Transaction,
    room: RoomId,
    root: &str,
    followed: bool,
    as_of: TxLogId,
    events: &mut Changes,
) -> Result<bool> {
    let at = this_entry(tx)?;
    let mut field = load(tx, room, root)?;
    let before = field.desired;
    field.observe(followed, as_of, at)?;
    store(tx, room, root, &field)?;
    if field.desired != before {
        shown(tx, room, root, field.desired, events)?;
    }
    Ok(field.desired != before)
}

/// The follow the account's op `by` asks of the room's thread of `root`,
/// unless asked for already: following on sending in it, or sending its
/// root (SPEC §9.2).
pub(crate) fn follow_on_send(
    tx: &Transaction,
    room: RoomId,
    root: &str,
    by: TxLogId,
) -> Result<Vec<ServerEvent>> {
    let mut field = load(tx, room, root)?;
    if field.desired {
        return Ok(vec![]);
    }
    field.want(true, by);
    store(tx, room, root, &field)?;
    let mut events = Changes::default();
    shown(tx, room, root, true, &mut events)?;
    Ok(events.into_events())
}

/// Whether the account's homeserver keeps thread subscriptions, as it
/// last synced (`SyncState::follows`).
pub(crate) fn followable(conn: &Connection, account: &str) -> Result<bool> {
    Ok(rooms::sync_state(conn, account)?.is_some_and(|s| s.follows))
}

// ── ops ───────────────────────────────────────────────────────────────────

/// Follows threads, or unfollows them. Changes already in effect are
/// skipped, so the inverse reverts exactly what this op changed (§6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFollows {
    pub changes: Vec<FollowChange>,
}

impl Op for SetFollows {
    const KIND: &'static str = "matrix.set_follows";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let this = this_entry(tx)?;
        let mut pending = BTreeMap::new();
        let mut inverse = vec![];
        let mut changes = Changes::default();
        for change in self.changes {
            let account: String = tx
                .query_row(
                    "SELECT account_id FROM matrix_room WHERE id = ?1",
                    [change.room.0],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Conflict("the room is no longer stored".into()))?;
            if let Entry::Vacant(vacant) = pending.entry(account) {
                let before = outbox::pending_rooms(tx, vacant.key())?;
                vacant.insert(before);
            }
            let mut field = load(tx, change.room, &change.root)?;
            let followed = field.desired;
            if followed == change.followed {
                continue;
            }
            field.want(change.followed, this);
            store(tx, change.room, &change.root, &field)?;
            shown(tx, change.room, &change.root, change.followed, &mut changes)?;
            inverse.push(FollowChange { followed, ..change });
        }
        let mut events = changes.into_events();
        for (account, before) in pending {
            if outbox::pending_rooms(tx, &account)? != before {
                events.push(outbox::outbox_changed(&account));
            }
        }
        // As the changes made, the inverse's being those undone.
        let followed = inverse.iter().filter(|c| !c.followed).count();
        let unfollowed = inverse.len() - followed;
        Ok(Applied {
            description: match (followed, unfollowed) {
                (0, 0) => "Changed nothing".to_owned(),
                (1, 0) => "Followed a thread".to_owned(),
                (0, 1) => "Unfollowed a thread".to_owned(),
                (n, 0) => format!("Followed {n} threads"),
                (0, n) => format!("Unfollowed {n} threads"),
                (f, u) => format!("Followed {f} threads and unfollowed {u}"),
            },
            inverse: Inverse::of(&SetFollows { changes: inverse }),
            events,
        })
    }
}

// ── the reconciler's ──────────────────────────────────────────────────────

/// The threads followed, each a field of its room, keyed by its root;
/// subscribed and unsubscribed one by one, and read back so.
pub struct Follows;

impl Kind for Follows {
    type Value = bool;
    /// The thread's root, and whether it is followed.
    type Read = (String, bool);
    /// The thread's root, and whether to follow it.
    type Request = (String, bool);

    const CLAIMED: &'static str = "matrix.follows_claimed";
    const PUSHED: &'static str = "matrix.follows_set";
    const READ: &'static str = "matrix.follows_read";
    const SETTLED: &'static str = "matrix.follow_pushes_settled";
    const WHAT: &'static str = "thread follows";
    const BY_KEY: bool = true;

    fn load(conn: &Connection, room: RoomId, root: &str) -> Result<Follow> {
        load(conn, room, root)
    }

    fn store(tx: &Transaction, room: RoomId, root: &str, field: &Follow) -> Result<()> {
        store(tx, room, root, field)
    }

    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<bool>>> {
        conn.prepare(
            "SELECT r.id, r.room_id, f.root, f.field
             FROM matrix_thread_follow f JOIN matrix_room r ON r.id = f.room
             WHERE r.account_id = ?1
               AND (f.desired_by IS NOT NULL OR json_extract(f.field, '$.check') IS NOT NULL)
             ORDER BY r.id, f.root",
        )?
        .query_map([account], |r| {
            Ok((
                RoomId(r.get(0)?),
                r.get(1)?,
                r.get(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (room, room_id, key, json) = row?;
            Ok(Stored {
                room,
                room_id,
                key,
                field: parse_field(&json)?,
            })
        })
        .collect()
    }

    fn observe(
        tx: &Transaction,
        room: RoomId,
        read: &(String, bool),
        as_of: TxLogId,
    ) -> Result<bool> {
        Ok(Self::observe_events(tx, room, read, as_of)?.0)
    }

    /// With its events: the root's, and its messages' that no longer
    /// notify.
    fn observe_events(
        tx: &Transaction,
        room: RoomId,
        (root, followed): &(String, bool),
        as_of: TxLogId,
    ) -> Result<(bool, Vec<ServerEvent>)> {
        let mut changes = Changes::default();
        let shown = observe_into(tx, room, root, *followed, as_of, &mut changes)?;
        Ok((shown, changes.into_events()))
    }

    fn pushed(room: &str, _root: &str, followed: &bool, ended: &Ended) -> String {
        let verb = if *followed { "follow" } else { "unfollow" };
        match ended {
            Ended::Applied if *followed => format!("Followed a thread of '{room}'"),
            Ended::Applied => format!("Unfollowed a thread of '{room}'"),
            Ended::NotApplied {
                refused: Some(error),
            } => format!("The homeserver refused to {verb} a thread of '{room}': {error}"),
            Ended::NotApplied { refused: None } => {
                format!("Could not {verb} a thread of '{room}' on the homeserver")
            }
            Ended::Unknown { .. } => {
                format!("Whether the homeserver did {verb} a thread of '{room}' is unknown")
            }
        }
    }

    fn requests(
        _conn: &Connection,
        in_flight: Vec<Stored<bool>>,
    ) -> Result<Requests<(String, bool), bool>> {
        Ok(reconciler::each(in_flight))
    }

    /// A manual subscription, which the homeserver takes over an
    /// automatic one; or none.
    async fn push(
        homeserver: &Homeserver,
        room: &RoomIdRef,
        (root, followed): &(String, bool),
    ) -> matrix_sdk::Result<()> {
        let root = <&EventId>::try_from(root.as_str())
            .map_err(|e| matrix_sdk::Error::UnknownError(Box::new(e)))?
            .to_owned();
        if *followed {
            homeserver
                .client
                .send(subscribe_thread::unstable::Request::new(
                    room.to_owned(),
                    root,
                    None,
                ))
                .await?;
        } else {
            homeserver
                .client
                .send(unsubscribe_thread::unstable::Request::new(
                    room.to_owned(),
                    root,
                ))
                .await?;
        }
        Ok(())
    }

    async fn read(homeserver: &Homeserver, room: &RoomIdRef, root: &str) -> Result<(String, bool)> {
        let followed = subscribed(&homeserver.client, room, <&EventId>::try_from(root)?).await?;
        Ok((root.to_owned(), followed))
    }
}

/// Whether the account is subscribed to the room's thread of `root`, as
/// the homeserver says: not if it says there is no such subscription, or
/// no such thread.
async fn subscribed(client: &Client, room: &RoomIdRef, root: &EventId) -> Result<bool> {
    let request = get_thread_subscription::unstable::Request::new(room.to_owned(), root.to_owned());
    match client.send(request).await {
        Ok(_) => Ok(true),
        Err(e) if e.client_api_error_kind() == Some(&ErrorKind::NotFound) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading whether {root} is followed")),
    }
}

/// A thread's follow pending, for the outbox.
pub(crate) struct Pending {
    pub account: String,
    /// The op that asked.
    pub op: TxLogId,
    pub change: FollowChange,
    /// Why it needs a decision, if it does.
    pub refused: Option<String>,
}

/// The follows pending, by account, then by the op that asked, newest
/// first.
pub(crate) fn pending(conn: &Connection) -> Result<Vec<Pending>> {
    conn.prepare(
        "SELECT r.account_id, f.room, f.root, f.field
         FROM matrix_thread_follow f JOIN matrix_room r ON r.id = f.room
         WHERE f.desired_by IS NOT NULL
         ORDER BY r.account_id, f.desired_by DESC, f.room, f.root",
    )?
    .query_map([], |r| {
        Ok((
            r.get(0)?,
            RoomId(r.get(1)?),
            r.get(2)?,
            r.get::<_, String>(3)?,
        ))
    })?
    .map(|row| {
        let (account, room, root, json) = row?;
        let field = parse_field(&json)?;
        Ok(Pending {
            account,
            op: field.opinion.context("pending without an opinion")?,
            change: FollowChange {
                room,
                root,
                followed: field.desired,
            },
            refused: field.decision.as_ref().map(|d| d.reason().to_owned()),
        })
    })
    .collect()
}

// ── the syncer's ──────────────────────────────────────────────────────────

/// Whether the homeserver keeps thread subscriptions (advertises
/// `org.matrix.msc4306`), as the client last asked it.
pub(crate) async fn supported(client: &Client) -> Result<bool> {
    Ok(client
        .supported_versions()
        .await
        .context("asking the homeserver what it supports")?
        .features
        .contains(&FeatureFlag::from("org.matrix.msc4306")))
}

/// Threads' follows the syncer read from the homeserver, or made by a
/// mention, by a request sent after `as_of` (SPEC §6.6.2).
#[derive(Debug, Serialize)]
pub struct FollowsSeen {
    pub as_of: TxLogId,
    pub seen: Vec<Seen>,
}

#[derive(Debug, Serialize)]
pub struct Seen {
    pub room_id: String,
    pub root: String,
    pub followed: bool,
    /// Whether a mention in it made it so (an automatic subscription).
    pub mentioned: bool,
}

impl Observation for FollowsSeen {
    const KIND: &'static str = "matrix.follows_seen";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let before = outbox::pending_rooms(tx, account)?;
        let mut changes = Changes::default();
        let mut mentioned = 0;
        for seen in &self.seen {
            let room: Option<i64> = tx
                .query_row(
                    "SELECT id FROM matrix_room WHERE account_id = ?1 AND room_id = ?2",
                    [account, &seen.room_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(room) = room else { continue };
            observe_into(
                tx,
                RoomId(room),
                &seen.root,
                seen.followed,
                self.as_of,
                &mut changes,
            )?;
            mentioned += usize::from(seen.mentioned && seen.followed);
        }
        let mut events = changes.into_events();
        if outbox::pending_rooms(tx, account)? != before {
            events.push(outbox::outbox_changed(account));
        }
        Ok(Observed {
            description: match (mentioned, self.seen.len()) {
                (0, 1) => "Read whether a thread is followed".to_owned(),
                (0, n) => format!("Read whether {n} threads are followed"),
                (1, _) => "Followed a thread one is mentioned in".to_owned(),
                (n, _) => format!("Followed {n} threads one is mentioned in"),
            },
            events,
        })
    }
}

/// Evaluates the push rules of messages of threads again, as the SDK's
/// evaluation does not know the threads followed: each thread as the
/// homeserver has it, read when a message of it comes, or as uplink asks
/// for it while it does; and follows a thread a message in it mentions
/// the account in, as the homeserver allows (SPEC §9.5).
pub(crate) struct Evaluator<'a> {
    pub client: &'a Client,
    /// Where the threads are as uplink has them; else, as `known`.
    pub reader: Option<&'a Arc<Mutex<Connection>>>,
    /// The threads followed, by root, unless read.
    pub known: HashMap<String, bool>,
    pub account: &'a str,
    /// Whether to read what the homeserver has of the threads uplink has
    /// no opinion of, and to follow those mentioned in: of a sync's news
    /// (sent since `news_since`), not of history filled in.
    pub sync: Option<i64>,
    /// What was read, or followed, to be recorded.
    pub seen: Vec<Seen>,
}

impl Evaluator<'_> {
    /// Of the room's `events` (as stored, of their `timeline`), those of
    /// threads have their push rules evaluated again.
    pub async fn evaluate(
        &mut self,
        room: &Room,
        events: &mut [ObservedEvent],
        timeline: &[TimelineEvent],
    ) -> Result<()> {
        // Of a root that is no event id, as another's client may send,
        // the push rules are the SDK's: none can be followed.
        let roots: BTreeSet<String> = events
            .iter()
            .filter(|e| e.relation.rel_type.as_deref() == Some("m.thread"))
            .filter_map(|e| e.relation.relates_to.clone())
            .filter(|root| <&EventId>::try_from(root.as_str()).is_ok())
            .collect();
        if roots.is_empty() || !supported(self.client).await? {
            return Ok(());
        }
        let Some(context) = room.push_condition_room_ctx().await? else {
            return Ok(());
        };
        let room_id = room.room_id().to_string();
        let (stored, pending) = match self.reader {
            Some(reader) => {
                let (account, room_id, roots) = (
                    self.account.to_owned(),
                    room_id.clone(),
                    roots.iter().cloned().collect::<Vec<_>>(),
                );
                read(reader, move |conn| {
                    shown_of(conn, &account, &room_id, &roots)
                })
                .await?
            }
            None => (self.known.clone(), BTreeSet::new()),
        };
        let mut followed: HashMap<String, bool> = stored;
        if self.sync.is_some() {
            for root in roots.iter().filter(|r| !pending.contains(*r)) {
                let id = <&EventId>::try_from(root.as_str()).expect("filtered");
                match subscribed(self.client, room.room_id(), id).await {
                    Ok(is) => {
                        followed.insert(root.clone(), is);
                        self.seen.push(Seen {
                            room_id: room_id.clone(),
                            root: root.clone(),
                            followed: is,
                            mentioned: false,
                        });
                    }
                    // As last known, rather than holding up the sync.
                    Err(e) => tracing::warn!(account = self.account, "{e:#}"),
                }
            }
        }
        let followed = Arc::new(Mutex::new(followed));
        let rules = self.client.account().push_rules().await?;
        let context = context.with_has_thread_subscription_fn({
            let followed = followed.clone();
            move |root: &EventId| {
                let is = followed
                    .lock()
                    .expect("not poisoned")
                    .get(root.as_str())
                    .copied()
                    .unwrap_or(false);
                Box::pin(async move { is })
            }
        });
        let push = PushContext::new(context, rules);
        let own = room.own_user_id().to_owned();
        for (event, timeline) in events.iter_mut().zip(timeline) {
            if event.relation.rel_type.as_deref() != Some("m.thread") {
                continue;
            }
            let Some(root) = event
                .relation
                .relates_to
                .clone()
                .filter(|r| roots.contains(r))
            else {
                continue;
            };
            let actions = push.for_event(timeline.raw()).await;
            event.push = Push::of(Some(actions.as_slice()));
            let Some(news_since) = self.sync else {
                continue;
            };
            let is_followed = followed.lock().expect("not poisoned").get(&root).copied();
            let mentions = event.push.notify
                && is_followed != Some(true)
                && !pending.contains(&root)
                && event.ts >= news_since
                && event.sender != own.as_str()
                && self.may_follow_for(room, &event.sender).await?;
            if !mentions {
                continue;
            }
            let Ok(cause) = <&EventId>::try_from(event.event_id.as_str()) else {
                continue;
            };
            let cause = cause.to_owned();
            if let Some(is) = self.follow_mentioned(room, &root, cause).await {
                followed
                    .lock()
                    .expect("not poisoned")
                    .insert(root.clone(), is);
                self.seen.push(Seen {
                    room_id: room_id.clone(),
                    root,
                    followed: is,
                    mentioned: true,
                });
            }
        }
        Ok(())
    }

    /// Whether a mention by `sender` follows its thread: not if the
    /// account ignores them, or they are banned (MSC4306).
    async fn may_follow_for(&self, room: &Room, sender: &str) -> Result<bool> {
        let sender = <&UserId>::try_from(sender)?;
        if self.client.is_user_ignored(sender).await {
            return Ok(false);
        }
        let banned = room
            .get_member_no_sync(sender)
            .await?
            .is_some_and(|m| *m.membership() == MembershipState::Ban);
        Ok(!banned)
    }

    /// Follows the room's thread of `root` for the mention `cause`;
    /// whether it is followed then: not if the user unfollowed it since
    /// (the homeserver skips the subscription). `None` if the homeserver
    /// could not be asked: a later mention asks again.
    async fn follow_mentioned(&self, room: &Room, root: &str, cause: OwnedEventId) -> Option<bool> {
        let root = <&EventId>::try_from(root).ok()?.to_owned();
        let request =
            subscribe_thread::unstable::Request::new(room.room_id().to_owned(), root, Some(cause));
        match self.client.send(request).await {
            Ok(_) => Some(true),
            Err(e) if e.client_api_error_kind() == Some(&ErrorKind::ConflictingUnsubscription) => {
                Some(false)
            }
            Err(e) => {
                tracing::warn!(
                    account = self.account,
                    "following a thread one is mentioned in: {e:#}"
                );
                None
            }
        }
    }
}

/// The room's threads followed, as shown, by root: what [`Evaluator`]
/// knows where it reads nothing.
pub fn followed(conn: &Connection, room: RoomId) -> Result<HashMap<String, bool>> {
    conn.prepare("SELECT root FROM matrix_thread_follow WHERE room = ?1 AND desired")?
        .query_map([room.0], |r| Ok((r.get(0)?, true)))?
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

/// Of the account's room (by its Matrix id), the threads of `roots` as
/// shown, followed or not, of those stored; and those uplink has an
/// opinion of, which stand until the homeserver has them.
fn shown_of(
    conn: &Connection,
    account: &str,
    room_id: &str,
    roots: &[String],
) -> Result<(HashMap<String, bool>, BTreeSet<String>)> {
    let mut shown = HashMap::new();
    let mut pending = BTreeSet::new();
    let mut query = conn.prepare(
        "SELECT f.root, f.desired, f.desired_by IS NOT NULL
         FROM matrix_thread_follow f JOIN matrix_room r ON r.id = f.room
         WHERE r.account_id = ?1 AND r.room_id = ?2
           AND f.root IN (SELECT value FROM json_each(?3))",
    )?;
    let rows = query.query_map(
        params![account, room_id, serde_json::to_string(roots)?],
        |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)),
    )?;
    for row in rows {
        let (root, desired, opinion): (String, bool, bool) = row?;
        if opinion {
            pending.insert(root.clone());
        }
        shown.insert(root, desired);
    }
    Ok((shown, pending))
}
