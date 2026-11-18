//! The reconciler (SPEC §6.6): brings an account's fields of a kind to
//! what the user asked for, as the [model](crate::reconcile) says, the
//! same for every provider. A [`Kind`] says how its fields are stored and
//! told of; a [`Provider`] pushes and reads them back. The observations
//! here record what the reconciler does to fields: claiming their pushes,
//! how each round of pushes ended, and pushes whose outcome became
//! unknown or expired.
//!
//! The provider's task calls [`read_back`] and [`push`] in turn: where
//! its sync takes turns with them, read back after the sync that follows
//! a push, for what that sync did not tell of; else, after each push.

use crate::{
    reconcile::{Ended, Field, Outcome},
    this_entry, Dispatcher, Observation, Observed,
};
use anyhow::{bail, ensure, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::BTreeSet,
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use uplink_protocol::ServerEvent;
use uplink_storage::rusqlite::{Connection, Transaction};

/// A kind of field: a mail entry's labels, a room's tags, an event's
/// answer.
pub trait Kind: Sized + Send + Sync + 'static {
    /// Which field: of an entity, which may have several of the kind.
    type Key: Clone + Ord + Debug + Serialize + Send + Sync + 'static;
    type Value: Clone + PartialEq + Debug + Serialize + DeserializeOwned + Send + Sync + 'static;
    /// What fields are of: a mail entry, a room, an event. Its fields are
    /// read back together, and it is what a push re-keys (SPEC §6.6.5).
    type Entity: Copy + Ord + Debug + Serialize + Send + Sync + 'static;
    /// What pushing or reading a field takes besides its key: its
    /// entity's ids at the provider.
    type Target: Clone + Debug + Send + Sync + 'static;
    /// The account's fields of the kind, in a transaction.
    type Fields<'t>: Fields<Self>;

    /// The kinds of the observations of the kind's fields.
    const CLAIMED: &'static str;
    const PUSHED: &'static str;
    const SETTLED: &'static str;

    fn entity(key: &Self::Key) -> Self::Entity;

    fn fields<'t>(tx: &'t Transaction<'t>, account: &'t str) -> Result<Self::Fields<'t>>;

    /// The account's fields uplink has an opinion of, or is to read back,
    /// or awaits a push of.
    fn reconciling(conn: &Connection, account: &str) -> Result<Vec<Stored<Self>>>;

    /// Whether the provider's key of the field's entity is known: a push
    /// that moved it without telling the new one leaves it unknown, until
    /// a sync finds it; until then, its fields are neither pushed nor read
    /// (SPEC §6.6.5).
    fn located(_target: &Self::Target) -> bool {
        true
    }

    /// Records the entity's key after a push applied changed it: `None`
    /// if the provider did not tell it.
    fn rekey(
        _tx: &Transaction,
        _account: &str,
        entity: Self::Entity,
        _external_id: Option<&str>,
    ) -> Result<()> {
        bail!("{entity:?} re-keyed, of a kind no push re-keys")
    }

    /// What changed for clients, of the fields that changed.
    fn events(
        tx: &Transaction,
        account: &str,
        changed: &[Changed<Self>],
    ) -> Result<Vec<ServerEvent>>;

    /// The log's description of the fields' pushes claimed.
    fn claimed(tx: &Transaction, claimed: &[Self::Key]) -> Result<String>;

    /// The log's description of how a round of pushes ended: each
    /// request's, with the fields it pushed, and the value of each.
    fn pushed(tx: &Transaction, pushes: &[(Ended, Values<Self>)]) -> Result<String>;

    /// The log's description of `settled` fields whose push became of
    /// unknown outcome (`interrupted`), or expired.
    fn settled(settled: usize, interrupted: bool) -> String;
}

/// Fields of a kind, each with a value of it.
pub type Values<K> = Vec<(<K as Kind>::Key, <K as Kind>::Value)>;

/// A kind's fields in a transaction.
pub trait Fields<K: Kind> {
    /// The field; `None` if its entity is gone.
    fn load(&self, key: &K::Key) -> Result<Option<Field<K::Value>>>;
    fn store(&mut self, key: &K::Key, field: &Field<K::Value>) -> Result<()>;
}

/// A field, as stored, of an account.
#[derive(Debug, Clone)]
pub struct Stored<K: Kind> {
    pub key: K::Key,
    pub target: K::Target,
    pub field: Field<K::Value>,
}

impl<K: Kind> Stored<K> {
    fn in_flight(&self) -> bool {
        self.field
            .attempt
            .as_ref()
            .is_some_and(|a| a.outcome == Outcome::InFlight)
    }
}

/// A field an observation here changed, as before and after.
pub struct Changed<K: Kind> {
    pub key: K::Key,
    pub before: Field<K::Value>,
    pub after: Field<K::Value>,
}

impl<K: Kind> Changed<K> {
    /// Whether what the outbox shows of the field changed: whether it is
    /// pending, as what, and its decision.
    pub fn pending(&self) -> bool {
        let pending = |f: &Field<K::Value>| {
            (
                f.opinion,
                f.opinion.map(|_| f.desired.clone()),
                f.decision.clone(),
            )
        };
        pending(&self.before) != pending(&self.after)
    }
}

/// What a provider supplies the reconciler of a kind: the account it
/// records to, and reads, and how it pushes and reads back fields.
pub trait Provider<K: Kind>: Send + Sync {
    fn account(&self) -> &str;
    fn dispatcher(&self) -> &Dispatcher;
    fn reader(&self) -> &Arc<Mutex<Connection>>;

    /// Pushes the fields in flight; how each request ended, of all of
    /// them, every failure classified (SPEC §6.6.3).
    fn push(&self, in_flight: Vec<Stored<K>>) -> impl Future<Output = Vec<PushEnded<K>>> + Send;

    /// Reads back the fields' entities, one field of each given, and
    /// records what it finds as observed by requests sent after the
    /// log's [`as_of`](crate::as_of) when each was sent.
    fn read(&self, entities: Vec<Stored<K>>) -> impl Future<Output = Result<()>> + Send;
}

/// How a request pushing fields ended.
#[derive(Debug, Serialize)]
#[serde(bound = "")]
pub struct Push<K: Kind> {
    pub fields: Vec<K::Key>,
    pub ended: Ended,
    /// The entity's key, if the push, applied, changed it (SPEC §6.6.5).
    pub rekeyed: Option<NewKey<K::Entity>>,
}

/// An entity's key after a push changed it: `None` if unknown, the
/// provider not telling, until a sync finds it again.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewKey<E> {
    pub entity: E,
    pub external_id: Option<String>,
}

/// How a request ended, and its failure, if any, that may pass.
pub struct PushEnded<K: Kind> {
    pub push: Push<K>,
    pub error: Option<anyhow::Error>,
}

/// Reads back the fields to be (SPEC §6.6.2), by entity; whether there
/// were any. One of an entity whose key is unknown waits on a sync to
/// find it, as does one a read does not find.
pub async fn read_back<K: Kind>(provider: &impl Provider<K>) -> Result<bool> {
    let mut entities = BTreeSet::new();
    let read: Vec<Stored<K>> = reconciling(provider)
        .await?
        .into_iter()
        .filter(|s| {
            K::located(&s.target) && s.field.read_back() && entities.insert(K::entity(&s.key))
        })
        .collect();
    if read.is_empty() {
        return Ok(false);
    }
    provider.read(read).await?;
    Ok(true)
}

/// Pushes the fields the user changed, in rounds, until none is left to
/// push but those whose push did not apply, which wait on the next call;
/// first, it settles pushes in flight, which a step or the process left
/// so, and those of unknown outcome long enough to expire. Whether a push
/// applied, or one expired: what became of it is to be confirmed. Every
/// push claimed ends recorded, though some fail; then the first failure,
/// that may pass, is returned, for the step to be retried with backoff.
pub async fn push<K: Kind>(provider: &impl Provider<K>) -> Result<bool> {
    // Each round pushes or settles; few are needed.
    const ROUNDS: usize = 8;
    let mut confirm = false;
    let mut later = BTreeSet::new();
    for _ in 0..ROUNDS {
        let fields = reconciling(provider).await?;
        let now = crate::now_ms();
        let in_flight = fields.iter().any(Stored::in_flight);
        if in_flight || fields.iter().any(|f| f.field.expiring(now)) {
            observe(provider, Settled::<K>::new(now, in_flight)).await?;
            confirm |= !in_flight;
            continue;
        }
        let wanted: Vec<K::Key> = fields
            .into_iter()
            .filter(|f| {
                K::located(&f.target)
                    && !later.contains(&K::entity(&f.key))
                    && f.field.wanted().is_some()
            })
            .map(|f| f.key)
            .collect();
        if wanted.is_empty() {
            return Ok(confirm);
        }
        observe(provider, Claimed::<K>::new(wanted)).await?;
        let mut claimed = reconciling(provider).await?;
        claimed.retain(Stored::in_flight);
        let mut failed = None;
        let mut pushes = vec![];
        for ended in provider.push(claimed).await {
            match ended.push.ended {
                Ended::Applied => confirm = true,
                Ended::NotApplied { refused: Some(_) } => {}
                // Not applied, or maybe: pushed again after the next
                // sync, rather than at once.
                Ended::NotApplied { refused: None } | Ended::Unknown { .. } => {
                    later.extend(ended.push.fields.iter().map(K::entity));
                }
            }
            if let Some(e) = ended.error {
                failed.get_or_insert(e);
            }
            pushes.push(ended.push);
        }
        // All ended by now: recorded as one.
        observe(provider, Pushed::<K> { pushes }).await?;
        if let Some(e) = failed {
            return Err(e);
        }
    }
    bail!("fields still to push after {ROUNDS} rounds")
}

async fn reconciling<K: Kind>(provider: &impl Provider<K>) -> Result<Vec<Stored<K>>> {
    let reader = provider.reader().clone();
    let account = provider.account().to_owned();
    tokio::task::spawn_blocking(move || {
        K::reconciling(&reader.lock().expect("reader poisoned"), &account)
    })
    .await?
}

async fn observe<K: Kind>(
    provider: &impl Provider<K>,
    observation: impl Observation,
) -> Result<()> {
    provider
        .dispatcher()
        .observe(provider.account().to_owned(), observation)
        .await
        .map(drop)
}

/// The fields whose pushes the reconciler claims, in one entry (SPEC
/// §6.6.3): those still wanted of `fields`.
#[derive(Debug, Serialize)]
#[serde(bound = "")]
pub struct Claimed<K: Kind> {
    pub fields: Vec<K::Key>,
    #[serde(skip)]
    kind: PhantomData<K>,
}

impl<K: Kind> Claimed<K> {
    pub fn new(fields: Vec<K::Key>) -> Self {
        Self {
            fields,
            kind: PhantomData,
        }
    }
}

impl<K: Kind> Observation for Claimed<K> {
    const KIND: &'static str = K::CLAIMED;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut fields = K::fields(tx, account)?;
        let mut claimed = vec![];
        for key in self.fields {
            // Unless gone, or settled by an op, meanwhile.
            let Some(mut field) = fields.load(&key)? else {
                continue;
            };
            if field.wanted().is_some() {
                field.claim()?;
                fields.store(&key, &field)?;
                claimed.push(key);
            }
        }
        Ok(Observed {
            description: K::claimed(tx, &claimed)?,
            events: vec![],
        })
    }
}

/// How a round of pushes ended: each request's, as the provider sent
/// them. A field gone meanwhile went with its entity.
#[derive(Debug, Serialize)]
#[serde(bound = "")]
pub struct Pushed<K: Kind> {
    pub pushes: Vec<Push<K>>,
}

impl<K: Kind> Observation for Pushed<K> {
    const KIND: &'static str = K::PUSHED;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let at = this_entry(tx)?;
        let mut fields = K::fields(tx, account)?;
        let mut changed = vec![];
        let mut pushed = vec![];
        for push in self.pushes {
            if let Some(key) = &push.rekeyed {
                ensure!(
                    push.ended == Ended::Applied,
                    "re-keyed by a push not applied"
                );
                K::rekey(tx, account, key.entity, key.external_id.as_deref())?;
            }
            let mut values = vec![];
            for key in push.fields {
                let Some(mut field) = fields.load(&key)? else {
                    continue;
                };
                let before = field.clone();
                let value = field
                    .attempt
                    .as_ref()
                    .map(|a| a.value.clone())
                    .ok_or_else(|| anyhow::anyhow!("{key:?} pushed, not claimed"))?;
                field.ended(push.ended.clone(), at)?;
                fields.store(&key, &field)?;
                values.push((key.clone(), value));
                changed.push(Changed {
                    key,
                    before,
                    after: field,
                });
            }
            pushed.push((push.ended, values));
        }
        Ok(Observed {
            description: K::pushed(tx, &pushed)?,
            events: K::events(tx, account, &changed)?,
        })
    }
}

/// Pushes whose outcome became unknown, as of `now_ms`: those in flight,
/// as a push stopped without telling how they ended (`interrupted`), else
/// those so long enough to expire (SPEC §6.6.3).
#[derive(Debug, Serialize)]
#[serde(bound = "")]
pub struct Settled<K: Kind> {
    pub now_ms: i64,
    pub interrupted: bool,
    #[serde(skip)]
    kind: PhantomData<K>,
}

impl<K: Kind> Settled<K> {
    pub fn new(now_ms: i64, interrupted: bool) -> Self {
        Self {
            now_ms,
            interrupted,
            kind: PhantomData,
        }
    }
}

impl<K: Kind> Observation for Settled<K> {
    const KIND: &'static str = K::SETTLED;

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let at = this_entry(tx)?;
        let stored = K::reconciling(tx, account)?;
        let mut fields = K::fields(tx, account)?;
        let mut changed = vec![];
        for Stored { key, mut field, .. } in stored {
            let before = field.clone();
            if self.interrupted {
                field.interrupted(self.now_ms);
            } else if field.expiring(self.now_ms) {
                field.expire(self.now_ms, at)?;
            }
            if field != before {
                fields.store(&key, &field)?;
                changed.push(Changed {
                    key,
                    before,
                    after: field,
                });
            }
        }
        Ok(Observed {
            description: K::settled(changed.len(), self.interrupted),
            events: K::events(tx, account, &changed)?,
        })
    }
}

/// Descriptions of parts of one entry, as one sentence: "Accepted 'A';
/// declined 'B'".
pub fn joined(parts: impl IntoIterator<Item = String>) -> String {
    let mut joined = String::new();
    for part in parts {
        if joined.is_empty() {
            joined = part;
        } else {
            let mut chars = part.chars();
            joined.push_str("; ");
            joined.extend(chars.next().map(|c| c.to_ascii_lowercase()));
            joined.push_str(chars.as_str());
        }
    }
    joined
}
