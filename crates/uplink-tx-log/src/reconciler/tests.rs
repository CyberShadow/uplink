//! The reconciler itself against a simulated provider (SPEC §15.10): as
//! the model's simulation, but through the driver, its observations and a
//! database: pushes applied, refused, refused at connect, timing out (and
//! applied, late or not at all), the process stopping mid-push; another
//! client; and syncs, either taking turns with pushes or a long poll
//! answered at any time, telling only of what changed. Random
//! interleavings from a seed, then the network quiet: every field must
//! converge, and the user's last intent win unless the provider could not
//! tell it from another's.

use super::*;
use crate::{
    reconcile::{Decision, UNKNOWN_FOR_MS},
    Registry,
};
use tokio::sync::Notify;
use uplink_protocol::TxLogId;
use uplink_realtime::Hub;
use uplink_storage::rusqlite::{params, OptionalExtension};

const KEYS: u8 = 2;
const VALUES: u8 = 4;

/// xorshift64*: reproducible from the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.next().is_multiple_of(n)
    }
}

/// Fields of a table of the simulation's, keyed by number; each its own
/// entity.
struct Sim;

struct SimFields<'t> {
    tx: &'t Transaction<'t>,
}

fn parse(json: &str) -> Field<u8> {
    serde_json::from_str(json).expect("a field")
}

impl Fields<Sim> for SimFields<'_> {
    fn load(&self, key: &u8) -> Result<Option<Field<u8>>> {
        Ok(self
            .tx
            .query_row("SELECT field FROM sim_field WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|json| parse(&json)))
    }

    fn store(&mut self, key: &u8, field: &Field<u8>) -> Result<()> {
        self.tx.execute(
            "UPDATE sim_field SET field = ?2 WHERE key = ?1",
            params![key, serde_json::to_string(field)?],
        )?;
        Ok(())
    }
}

fn fields(conn: &Connection) -> Vec<(u8, Field<u8>)> {
    conn.prepare("SELECT key, field FROM sim_field ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, parse(&r.get::<_, String>(1)?))))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

impl Kind for Sim {
    type Key = u8;
    type Value = u8;
    type Entity = u8;
    type Target = ();
    type Fields<'t> = SimFields<'t>;

    const CLAIMED: &'static str = "sim.claimed";
    const PUSHED: &'static str = "sim.pushed";
    const SETTLED: &'static str = "sim.settled";

    fn entity(key: &u8) -> u8 {
        *key
    }

    fn fields<'t>(tx: &'t Transaction<'t>, _account: &'t str) -> Result<SimFields<'t>> {
        Ok(SimFields { tx })
    }

    /// As every kind's: of an opinion, or to be read back.
    fn reconciling(conn: &Connection, _account: &str) -> Result<Vec<Stored<Self>>> {
        Ok(fields(conn)
            .into_iter()
            .filter(|(_, f)| f.opinion.is_some() || f.check.is_some())
            .map(|(key, field)| Stored {
                key,
                target: (),
                field,
            })
            .collect())
    }

    fn events(_: &Transaction, _: &str, _: &[Changed<Self>]) -> Result<Vec<ServerEvent>> {
        Ok(vec![])
    }

    fn claimed(_: &Transaction, claimed: &[u8]) -> Result<String> {
        Ok(format!("claimed {claimed:?}"))
    }

    fn pushed(_: &Transaction, pushes: &[(Ended, Values<Self>)]) -> Result<String> {
        Ok(format!("pushed {pushes:?}"))
    }

    fn settled(settled: usize, interrupted: bool) -> String {
        format!("settled {settled}, interrupted: {interrupted}")
    }
}

/// The user asks for `value`.
#[derive(Serialize)]
struct Want {
    key: u8,
    value: u8,
}

impl Observation for Want {
    const KIND: &'static str = "sim.want";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut fields = Sim::fields(tx, account)?;
        let mut field = fields.load(&self.key)?.expect("a field");
        field.want(self.value, this_entry(tx)?);
        fields.store(&self.key, &field)?;
        Ok(Observed {
            description: format!("want {}", self.value),
            events: vec![],
        })
    }
}

/// The provider was seen to have `value`, by a request sent after `as_of`.
#[derive(Serialize)]
struct Seen {
    key: u8,
    value: u8,
    as_of: TxLogId,
}

impl Observation for Seen {
    const KIND: &'static str = "sim.seen";

    fn apply(self, account: &str, tx: &Transaction) -> Result<Observed> {
        let mut fields = Sim::fields(tx, account)?;
        let mut field = fields.load(&self.key)?.expect("a field");
        field.observe(self.value, self.as_of, this_entry(tx)?)?;
        fields.store(&self.key, &field)?;
        Ok(Observed {
            description: format!("seen {} as of {}", self.value, self.as_of.0),
            events: vec![],
        })
    }
}

/// What the provider does with values, and how its sync tells of them.
#[derive(Debug, Clone, Copy)]
struct Config {
    /// Kept as 0.
    not_kept: Option<u8>,
    refused: Option<u8>,
    /// Whether another client changes values too.
    others: bool,
    /// Whether the sync is a long poll beside the pushes (Matrix), rather
    /// than taking turns with them.
    long_poll: bool,
}

/// A long poll: sent after `as_of`, for changes since `since`; answered
/// with what changed, as of the version then.
struct Poll {
    as_of: TxLogId,
    since: u64,
    answer: Option<(Vec<(u8, u8)>, u64)>,
}

struct World {
    rng: Rng,
    config: Config,
    faults: bool,
    /// The clock, in ms: a second a step.
    ms: i64,
    values: [u8; KEYS as usize],
    /// The version of each value, and of all.
    changed: [u64; KEYS as usize],
    version: u64,
    /// The version the client's sync is recorded up to.
    synced: u64,
    poll: Option<Poll>,
    /// Writes of pushes given up on, which may still land, and since when.
    stragglers: Vec<(u8, u8, i64)>,
    /// The user's last intent, and whether another client wrote since.
    last_want: [Option<u8>; KEYS as usize],
    other_since_want: [bool; KEYS as usize],
}

impl World {
    fn write(&mut self, key: u8, value: u8) {
        self.version += 1;
        self.values[key as usize] = value;
        self.changed[key as usize] = self.version;
    }

    fn kept(&self, value: u8) -> u8 {
        if self.config.not_kept == Some(value) {
            0
        } else {
            value
        }
    }

    /// A write that may still land: now, later or never.
    fn maybe(&mut self, key: u8, value: u8) {
        match self.rng.below(3) {
            0 => self.write(key, value),
            1 => self.stragglers.push((key, value, self.ms)),
            _ => {}
        }
    }

    /// Serves a push of `value`; whether the process stops meanwhile.
    fn push(&mut self, key: u8, value: u8) -> (PushEnded<Sim>, bool) {
        let ended = |ended, error: Option<&str>| PushEnded {
            push: Push {
                fields: vec![key],
                ended,
                rekeyed: None,
            },
            error: error.map(|e| anyhow::anyhow!("{e}")),
        };
        if self.faults && self.rng.one_in(8) {
            let refused = Ended::NotApplied { refused: None };
            return (ended(refused, Some("refused at connect")), false);
        }
        if self.config.refused == Some(value) {
            let refused = Ended::NotApplied {
                refused: Some("refused".into()),
            };
            return (ended(refused, None), false);
        }
        let kept = self.kept(value);
        match if self.faults { self.rng.below(10) } else { 9 } {
            0 => {
                self.maybe(key, kept);
                (ended(Ended::Applied, None), true)
            }
            1 => {
                self.maybe(key, kept);
                let unknown = Ended::Unknown { at_ms: self.ms };
                (ended(unknown, Some("timed out")), false)
            }
            _ => {
                self.write(key, kept);
                (ended(Ended::Applied, None), false)
            }
        }
    }

    /// What changed since `since`.
    fn changes(&self, since: u64) -> Vec<(u8, u8)> {
        (0..KEYS)
            .filter(|&k| self.changed[k as usize] > since)
            .map(|k| (k, self.values[k as usize]))
            .collect()
    }

    /// Stragglers given up on long enough ago land no more.
    fn expire(&mut self) {
        let ms = self.ms;
        self.stragglers
            .retain(|&(_, _, since)| ms - since < UNKNOWN_FOR_MS);
    }
}

/// The simulated provider, as the reconciler sees it.
struct Net {
    world: std::sync::Mutex<World>,
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    /// Told as the process stops mid-push.
    stopped: Notify,
}

impl Provider<Sim> for Net {
    fn account(&self) -> &str {
        "sim"
    }

    fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    fn reader(&self) -> &Arc<Mutex<Connection>> {
        &self.reader
    }

    fn now_ms(&self) -> i64 {
        self.world.lock().unwrap().ms
    }

    async fn push(&self, in_flight: Vec<Stored<Sim>>) -> Vec<PushEnded<Sim>> {
        let mut pushes = vec![];
        for stored in in_flight {
            let value = stored.field.attempt.expect("in flight").value;
            let (ended, stopped) = self.world.lock().unwrap().push(stored.key, value);
            if stopped {
                self.stopped.notify_one();
                std::future::pending::<()>().await;
            }
            pushes.push(ended);
        }
        pushes
    }

    async fn read(&self, entities: Vec<Stored<Sim>>) -> Result<()> {
        for stored in entities {
            let as_of = self.as_of();
            let value = self.world.lock().unwrap().values[stored.key as usize];
            self.see(stored.key, value, as_of).await;
        }
        Ok(())
    }
}

impl Net {
    fn as_of(&self) -> TxLogId {
        crate::as_of(&self.reader.lock().unwrap()).unwrap()
    }

    async fn see(&self, key: u8, value: u8, as_of: TxLogId) {
        self.observe(Seen { key, value, as_of }).await;
    }

    async fn observe(&self, observation: impl Observation) {
        self.dispatcher
            .observe("sim".into(), observation)
            .await
            .unwrap();
    }

    fn fields(&self) -> Vec<(u8, Field<u8>)> {
        fields(&self.reader.lock().unwrap())
    }

    /// Pushes, unless the process stops meanwhile: then what it left in
    /// flight is for the next push to settle, and its long poll is not
    /// heard.
    async fn push(&self) {
        tokio::select! {
            biased;
            () = self.stopped.notified() => self.world.lock().unwrap().poll = None,
            // A failure that may pass: the task retries.
            _ = super::push::<Sim>(self) => {}
        }
    }

    async fn read_back(&self) {
        super::read_back::<Sim>(self).await.unwrap();
    }

    /// A sync that takes turns with pushes: sent after what is recorded.
    async fn sync(&self) {
        let as_of = self.as_of();
        let (changes, version) = {
            let w = self.world.lock().unwrap();
            (w.changes(w.synced), w.version)
        };
        for (key, value) in changes {
            self.see(key, value, as_of).await;
        }
        self.world.lock().unwrap().synced = version;
    }

    /// A long poll's step: sent, answered once something changed (or
    /// timing out, if `timeout`), or its answer recorded.
    async fn poll(&self, timeout: bool) {
        let as_of = self.as_of();
        let answered = {
            let mut w = self.world.lock().unwrap();
            let (synced, version) = (w.synced, w.version);
            let changes = w.changes(synced);
            match &mut w.poll {
                None => {
                    w.poll = Some(Poll {
                        as_of,
                        since: synced,
                        answer: None,
                    });
                    None
                }
                Some(Poll { answer: None, .. }) if version > synced || timeout => {
                    w.poll.as_mut().unwrap().answer = Some((changes, version));
                    None
                }
                Some(Poll { answer: None, .. }) => None,
                Some(Poll {
                    answer: Some(_), ..
                }) => w.poll.take(),
            }
        };
        if let Some(Poll {
            as_of,
            since,
            answer: Some((changes, version)),
        }) = answered
        {
            assert!(since <= version);
            for (key, value) in changes {
                self.see(key, value, as_of).await;
            }
            self.world.lock().unwrap().synced = version;
        }
    }

    /// A step of the world, at random.
    async fn step(&self) {
        let (choice, faults, config) = {
            let mut w = self.world.lock().unwrap();
            w.ms += 1000;
            w.expire();
            (w.rng.below(9), w.faults, w.config)
        };
        match choice {
            0 if faults => {
                let (key, value) = {
                    let mut w = self.world.lock().unwrap();
                    (
                        w.rng.below(KEYS as usize) as u8,
                        w.rng.below(VALUES as usize) as u8,
                    )
                };
                let before = self.fields()[key as usize].1.desired;
                self.observe(Want { key, value }).await;
                // An op that changes nothing is no new intent. One that
                // asks for what is shown pushes nothing: another's change
                // not yet seen still wins over it.
                if value != before {
                    let pending = self.fields()[key as usize].1.pending();
                    let mut w = self.world.lock().unwrap();
                    w.last_want[key as usize] = Some(value);
                    if pending {
                        w.other_since_want[key as usize] = false;
                    }
                }
            }
            1 if faults && config.others => {
                let mut w = self.world.lock().unwrap();
                if w.rng.one_in(3) {
                    let key = w.rng.below(KEYS as usize) as u8;
                    let value = w.rng.below(VALUES as usize) as u8;
                    w.write(key, value);
                    w.other_since_want[key as usize] = true;
                }
            }
            2 => {
                let mut w = self.world.lock().unwrap();
                if !w.stragglers.is_empty() && w.rng.one_in(3) {
                    let landing = w.stragglers.len();
                    let i = w.rng.below(landing);
                    let (key, value, _) = w.stragglers.swap_remove(i);
                    w.write(key, value);
                }
            }
            3 | 4 if config.long_poll => {
                let timeout = self.world.lock().unwrap().rng.one_in(4);
                self.poll(timeout).await;
            }
            3 | 4 => self.sync().await,
            5 | 6 => self.push().await,
            _ => self.read_back().await,
        }
    }

    /// Whether nothing is left to do, the network quiet.
    fn quiet(&self) -> bool {
        let w = self.world.lock().unwrap();
        w.stragglers.is_empty()
            && w.poll.is_none()
            && w.synced == w.version
            && self.fields().iter().all(|(_, f)| {
                f.wanted().is_none()
                    && !f.read_back()
                    && !f.expiring(i64::MAX)
                    && f.attempt.is_none()
            })
    }
}

/// Runs a seed; returns whether the user's intent was checked (else only
/// convergence).
async fn run(seed: u64, config: Config) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let conn = uplink_storage::open(dir.path()).unwrap();
    conn.execute_batch("CREATE TABLE sim_field (key INTEGER PRIMARY KEY, field TEXT NOT NULL)")
        .unwrap();
    for key in 0..KEYS {
        conn.execute(
            "INSERT INTO sim_field (key, field) VALUES (?1, ?2)",
            params![
                key,
                serde_json::to_string(&Field::new(0u8, TxLogId(0))).unwrap()
            ],
        )
        .unwrap();
    }
    let net = Net {
        world: std::sync::Mutex::new(World {
            rng: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
            config,
            faults: true,
            ms: 0,
            values: [0; KEYS as usize],
            changed: [0; KEYS as usize],
            version: 0,
            synced: 0,
            poll: None,
            stragglers: vec![],
            last_want: [None; KEYS as usize],
            other_since_want: [false; KEYS as usize],
        }),
        dispatcher: Dispatcher::new(conn, Registry::new(), Arc::new(Hub::new(64))),
        reader: Arc::new(Mutex::new(uplink_storage::open_reader(dir.path()).unwrap())),
        stopped: Notify::new(),
    };
    for _ in 0..200 {
        net.step().await;
    }
    net.world.lock().unwrap().faults = false;
    let mut steps = 0;
    while !net.quiet() {
        steps += 1;
        assert!(
            steps < 5_000,
            "seed {seed}, {config:?}: no convergence: {:#?}",
            net.fields()
        );
        net.step().await;
    }
    let w = net.world.lock().unwrap();
    let mut checked = false;
    for (key, f) in net.fields() {
        let k = key as usize;
        let value = w.values[k];
        let context = || format!("seed {seed}, {config:?}, key {key}, server {value}: {f:#?}");
        match &f.decision {
            None => {
                assert!(!f.pending(), "{}", context());
                assert_eq!((f.desired, f.observed), (value, value), "{}", context());
            }
            Some(_) => assert!(f.pending(), "{}", context()),
        }
        // The user's last intent wins, unless another client wrote since.
        let (Some(want), false) = (w.last_want[k], w.other_since_want[k]) else {
            continue;
        };
        checked = true;
        // Had already (another client's), nothing is pushed.
        let expected = if want == value {
            None
        } else if config.refused == Some(want) {
            Some(Decision::Refused("refused".into()))
        } else if config.not_kept == Some(want) {
            Some(Decision::NotKept)
        } else {
            None
        };
        assert_eq!(f.decision, expected, "{}", context());
        if expected.is_none() {
            assert_eq!(value, want, "{}", context());
        }
    }
    checked
}

/// Runs `seeds` of `config`, of which at least `share` must check the
/// user's intent.
fn sweep(seeds: std::ops::Range<u64>, config: Config, share: f64) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let total = seeds.end - seeds.start;
    let mut checked = 0;
    for seed in seeds {
        checked += u64::from(runtime.block_on(run(seed, config)));
    }
    let msg = format!("{checked} of {total} runs checked the user's intent, {config:?}");
    eprintln!("{msg}");
    assert!(checked as f64 >= share * total as f64, "{msg}");
}

const SEEDS: std::ops::Range<u64> = 0..100;

#[test]
fn every_interleaving_converges_to_the_users_intent() {
    for long_poll in [false, true] {
        let config = Config {
            not_kept: None,
            refused: None,
            others: false,
            long_poll,
        };
        sweep(SEEDS, config, 0.9);
    }
}

#[test]
fn values_not_kept_or_refused_need_a_decision() {
    for long_poll in [false, true] {
        let config = Config {
            not_kept: Some(3),
            refused: Some(2),
            others: false,
            long_poll,
        };
        sweep(SEEDS, config, 0.9);
    }
}

#[test]
fn another_clients_changes_converge() {
    for long_poll in [false, true] {
        let config = Config {
            not_kept: Some(3),
            refused: Some(2),
            others: true,
            long_poll,
        };
        sweep(SEEDS, config, 0.3);
    }
}
