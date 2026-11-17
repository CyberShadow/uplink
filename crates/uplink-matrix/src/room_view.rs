//! The rooms, as [`rooms::rooms`] lists them, kept by the writer as a
//! view of the database (SPEC §9.5): what lists rooms, matches them to
//! queries (windows, badges) or checks requests of them takes them as they
//! are, rather than counting each room's messages anew each time.

use crate::{rooms, service::ConfiguredAccount};
use anyhow::Result;
use arc_swap::ArcSwap;
use std::{cmp::Reverse, sync::Arc};
use uplink_protocol::{
    matrix::{Room, RoomId},
    Changed, LogEntry, TxLogId,
};
use uplink_storage::rusqlite::Transaction;
use uplink_tx_log::{Dispatcher, View};

pub struct RoomsView {
    accounts: Arc<Vec<ConfiguredAccount>>,
    listed: Arc<ArcSwap<Listed>>,
}

/// Every account's rooms, of each account in turn, as [`rooms::rooms`]
/// lists them as of entry `as_of`.
pub struct Listed {
    pub as_of: TxLogId,
    pub rooms: Vec<Room>,
}

impl RoomsView {
    /// Of `accounts`, kept by `dispatcher`, the only writer.
    pub fn kept(dispatcher: &Dispatcher, accounts: Vec<ConfiguredAccount>) -> Result<Arc<Self>> {
        let accounts = Arc::new(accounts);
        dispatcher.keep(|conn| {
            let tx = conn.unchecked_transaction()?;
            Ok(Self {
                listed: Arc::new(ArcSwap::from_pointee(Listed {
                    as_of: uplink_tx_log::as_of(&tx)?,
                    rooms: listed(&tx, &accounts, None)?,
                })),
                accounts,
            })
        })
    }

    pub fn accounts(&self) -> &Arc<Vec<ConfiguredAccount>> {
        &self.accounts
    }

    /// As of the latest entry told of, or a later one.
    pub fn listed(&self) -> Arc<Listed> {
        self.listed.load_full()
    }
}

/// The rooms of `accounts` as listed, of each account in turn; of those,
/// `only` these, if given.
fn listed(
    tx: &Transaction,
    accounts: &[ConfiguredAccount],
    only: Option<&[RoomId]>,
) -> Result<Vec<Room>> {
    let mut all = vec![];
    for account in accounts {
        all.extend(rooms::rooms(tx, account, only)?);
    }
    Ok(all)
}

impl View for RoomsView {
    /// The rooms the entry names listed anew (each event of a room names
    /// it), the others as they were.
    fn derive(&self, tx: &Transaction, entry: &LogEntry) -> Result<Box<dyn FnOnce() + Send>> {
        let mut changed = Changed::default();
        changed.came(entry);
        let mut named = changed.rooms;
        named.sort();
        named.dedup();
        if named.is_empty() {
            return Ok(Box::new(|| {}));
        }
        let fresh = listed(tx, &self.accounts, Some(&named))?;
        let mut rooms: Vec<Room> = self
            .listed
            .load()
            .rooms
            .iter()
            .filter(|room| named.binary_search(&room.id).is_err())
            .cloned()
            .chain(fresh)
            .collect();
        // As `rooms::rooms` orders them, of each account in turn.
        let account = |room: &Room| {
            self.accounts
                .iter()
                .position(|a| a.id == room.account)
                .expect("a room is of a configured account")
        };
        rooms.sort_by_key(|room| {
            (
                account(room),
                room.last_event_ts.is_none(),
                Reverse(room.last_event_ts),
                room.id,
            )
        });
        // Every change to a room names it, or what is kept is stale: checked
        // as the tests write.
        if cfg!(debug_assertions) {
            assert_eq!(
                rooms,
                listed(tx, &self.accounts, None)?,
                "the rooms kept after {} are as listed anew",
                entry.description
            );
        }
        let listed = self.listed.clone();
        let as_of = entry.id;
        Ok(Box::new(move || {
            listed.store(Arc::new(Listed { as_of, rooms }))
        }))
    }
}
