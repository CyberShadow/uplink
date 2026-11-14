//! The model against a simulated provider (SPEC §15.10): incremental sync
//! by long poll, as Matrix's, beside pushes and read-backs; requests and
//! answers delivered in any order; pushes refused at connect, timing out
//! (applied or not, or applied late), refused, or kept as another value;
//! another client changing the value; the process restarting. Random
//! interleavings from a seed, then the network quiet: every run must
//! converge, and the user's last intent must win unless the provider
//! could not tell it from another's.

use super::super::*;

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

const VALUES: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Req {
    Write { value: u8, push: u64, answer: bool },
    Read { as_of: TxLogId },
    Sync { as_of: TxLogId, since: u64 },
}

#[derive(Debug)]
enum Answer {
    Write {
        push: u64,
        ended: Ended,
    },
    Read {
        as_of: TxLogId,
        value: u8,
    },
    Sync {
        as_of: TxLogId,
        changed: Option<u8>,
        version: u64,
    },
}

/// A request or answer of the process's `epoch`: one of an earlier run of
/// it is ignored.
struct Sent<T> {
    epoch: u32,
    item: T,
}

/// What the provider does with values: keeps them, or some as others, or
/// refuses some.
#[derive(Debug, Clone, Copy)]
struct Provider {
    /// Kept as 0.
    not_kept: Option<u8>,
    refused: Option<u8>,
    /// Whether another client changes the value too.
    others: bool,
}

struct World {
    rng: Rng,
    provider: Provider,
    value: u8,
    version: u64,
    to_server: Vec<Sent<Req>>,
    to_client: Vec<Sent<Answer>>,
    /// Long polls waiting for a change.
    parked: Vec<Sent<Req>>,

    field: Field<u8>,
    log: i64,
    epoch: u32,
    /// The push in flight, by its number.
    pushing: Option<u64>,
    pushes: u64,
    /// Pushes the client gave up on, which may still land, and since when.
    given_up: Vec<(u64, i64)>,
    /// The clock, in ms: a second a step.
    ms: i64,
    reading: bool,
    syncing: bool,
    since: u64,

    /// Whether another client wrote since the user's last op.
    other_since_want: bool,
    last_want: Option<u8>,
    /// Whether to print what happens (`UPLINK_SIM_TRACE` set to the seed).
    trace: bool,
}

macro_rules! trace {
    ($w:expr, $($arg:tt)*) => {
        if $w.trace {
            eprintln!("{:>4} {}", $w.log, format!($($arg)*));
        }
    };
}

impl World {
    fn new(seed: u64, provider: Provider) -> Self {
        Self {
            rng: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
            provider,
            value: 0,
            version: 0,
            to_server: vec![],
            to_client: vec![],
            parked: vec![],
            field: Field::new(0, TxLogId(0)),
            log: 0,
            epoch: 0,
            pushing: None,
            pushes: 0,
            given_up: vec![],
            ms: 0,
            reading: false,
            syncing: false,
            since: 0,
            other_since_want: false,
            last_want: None,
            trace: std::env::var("UPLINK_SIM_TRACE").is_ok_and(|s| s == seed.to_string()),
        }
    }

    fn entry(&mut self) -> TxLogId {
        self.log += 1;
        TxLogId(self.log)
    }

    fn now(&self) -> TxLogId {
        TxLogId(self.log)
    }

    fn send(&mut self, req: Req) {
        let epoch = self.epoch;
        self.to_server.push(Sent { epoch, item: req });
    }

    fn answer(&mut self, epoch: u32, answer: Answer) {
        self.to_client.push(Sent {
            epoch,
            item: answer,
        });
    }

    fn changed(&mut self) {
        self.version += 1;
        for parked in std::mem::take(&mut self.parked) {
            self.serve(parked);
        }
    }

    fn serve(&mut self, sent: Sent<Req>) {
        let Sent { epoch, item } = sent;
        match item {
            Req::Write {
                value,
                push,
                answer,
            } => {
                if self.provider.refused == Some(value) {
                    let ended = Ended::NotApplied {
                        refused: Some("refused".into()),
                    };
                    self.answer(epoch, Answer::Write { push, ended });
                    return;
                }
                trace!(self, "SERVER applies push {push} of {value}");
                self.value = if self.provider.not_kept == Some(value) {
                    0
                } else {
                    value
                };
                self.changed();
                if answer {
                    let at = TxLogId(0); // set by the client as it records
                    self.answer(
                        epoch,
                        Answer::Write {
                            push,
                            ended: Ended::Applied { at },
                        },
                    );
                }
            }
            Req::Read { as_of } => {
                let value = self.value;
                self.answer(epoch, Answer::Read { as_of, value });
            }
            Req::Sync { as_of, since } => {
                if self.version > since {
                    let (changed, version) = (Some(self.value), self.version);
                    self.answer(
                        epoch,
                        Answer::Sync {
                            as_of,
                            changed,
                            version,
                        },
                    );
                } else {
                    self.parked.push(Sent { epoch, item });
                }
            }
        }
    }

    /// The long poll times out, unchanged.
    fn poll_timeout(&mut self) {
        for Sent { epoch, item } in std::mem::take(&mut self.parked) {
            let Req::Sync { as_of, .. } = item else {
                unreachable!()
            };
            let version = self.version;
            self.answer(
                epoch,
                Answer::Sync {
                    as_of,
                    changed: None,
                    version,
                },
            );
        }
    }

    fn receive(&mut self, sent: Sent<Answer>) {
        if sent.epoch != self.epoch {
            return;
        }
        match sent.item {
            Answer::Write { push, ended } => {
                if self.pushing != Some(push) {
                    return; // given up on
                }
                self.pushing = None;
                let ended = match ended {
                    Ended::Applied { .. } => Ended::Applied { at: self.entry() },
                    other => {
                        self.entry();
                        other
                    }
                };
                {
                    trace!(self, "ended {ended:?}");
                    self.field.ended(ended).unwrap();
                    trace!(self, "  {:?}", self.field);
                }
            }
            Answer::Read { as_of, value } => {
                self.reading = false;
                let at = self.entry();
                {
                    trace!(
                        self,
                        "observe {value} as_of {} (server {} v{})",
                        as_of.0,
                        self.value,
                        self.version
                    );
                    self.field.observe(value, as_of, at).unwrap();
                    trace!(self, "  {:?}", self.field);
                }
            }
            Answer::Sync {
                as_of,
                changed,
                version,
            } => {
                self.syncing = false;
                self.since = version;
                if let Some(value) = changed {
                    let at = self.entry();
                    {
                        trace!(
                            self,
                            "observe {value} as_of {} (server {} v{})",
                            as_of.0,
                            self.value,
                            self.version
                        );
                        self.field.observe(value, as_of, at).unwrap();
                        trace!(self, "  {:?}", self.field);
                    }
                }
            }
        }
    }

    /// The reconciler's step: push, read back, sync, as the model asks.
    fn reconcile(&mut self, faults: bool) {
        if self.pushing.is_none() && self.field.wanted().is_some() {
            self.entry();
            let value = self.field.claim().unwrap();
            trace!(self, "claim {value}");
            self.pushes += 1;
            let push = self.pushes;
            if faults && self.rng.one_in(8) {
                // Refused at connect: never sent.
                self.entry();
                let ended = Ended::NotApplied { refused: None };
                {
                    trace!(self, "ended {ended:?}");
                    self.field.ended(ended).unwrap();
                    trace!(self, "  {:?}", self.field);
                }
                return;
            }
            self.pushing = Some(push);
            let answer = !(faults && self.rng.one_in(6));
            self.send(Req::Write {
                value,
                push,
                answer,
            });
        }
        if !self.reading && self.field.read_back() {
            self.reading = true;
            let as_of = self.now();
            self.send(Req::Read { as_of });
        }
        if !self.syncing {
            self.syncing = true;
            let (as_of, since) = (self.now(), self.since);
            self.send(Req::Sync { as_of, since });
        }
    }

    /// Pushes given up on long enough ago are done with: they land no
    /// more, and the model is told.
    fn expire(&mut self) {
        let done: Vec<u64> = self
            .given_up
            .iter()
            .filter(|&&(_, since)| self.ms - since >= UNKNOWN_FOR_MS)
            .map(|&(p, _)| p)
            .collect();
        self.to_server
            .retain(|s| !matches!(s.item, Req::Write { push, .. } if done.contains(&push)));
        self.given_up.retain(|(p, _)| !done.contains(p));
        if self.field.expiring(self.ms) {
            let at = self.entry();
            trace!(self, "expired");
            self.field.expire(self.ms, at).unwrap();
        }
    }

    fn step(&mut self, faults: bool) {
        self.ms += 1000;
        self.expire();
        match self.rng.below(10) {
            0 if faults => {
                let value = self.rng.below(VALUES as usize) as u8;
                let by = self.entry();
                let before = self.field.desired;
                trace!(self, "want {value}");
                self.field.want(value, by);
                // An op that changes nothing is no new intent. One that
                // asks for what is shown pushes nothing: another's change
                // not yet seen still wins over it.
                if value != before {
                    self.last_want = Some(value);
                    if self.field.pending() {
                        self.other_since_want = false;
                    }
                }
            }
            1 if faults && self.provider.others && self.rng.one_in(3) => {
                self.value = self.rng.below(VALUES as usize) as u8;
                trace!(self, "OTHER writes {}", self.value);
                self.other_since_want = true;
                self.changed();
            }
            2 if faults && self.pushing.is_some() && self.rng.one_in(3) => {
                // The client gives up on the push: outcome unknown.
                let push = self.pushing.take().unwrap();
                self.given_up.push((push, self.ms));
                self.entry();
                {
                    trace!(self, "gave up");
                    self.field.ended(Ended::Unknown { at_ms: self.ms }).unwrap();
                }
            }
            3 if faults && self.rng.one_in(10) => {
                // The process restarts: what was in flight is unknown, and
                // answers to its requests are not heard.
                self.epoch += 1;
                if let Some(push) = self.pushing.take() {
                    self.given_up.push((push, self.ms));
                }
                trace!(self, "RESTART");
                self.field.interrupted(self.ms);
                self.reading = false;
                self.syncing = false;
            }
            4 if faults && !self.to_server.is_empty() && self.rng.one_in(6) => {
                // A request lost on the way.
                let i = self.rng.below(self.to_server.len());
                self.to_server.swap_remove(i);
            }
            5 => self.poll_timeout(),
            6 | 7 if !self.to_server.is_empty() => {
                let i = self.rng.below(self.to_server.len());
                let sent = self.to_server.swap_remove(i);
                self.serve(sent);
            }
            8 | 9 if !self.to_client.is_empty() => {
                // Answers in any order, as of concurrent requests.
                let i = self.rng.below(self.to_client.len());
                let sent = self.to_client.swap_remove(i);
                self.receive(sent);
            }
            _ => {}
        }
        self.reconcile(faults);
    }

    /// Whether nothing is left to do, the network quiet.
    fn quiet(&self) -> bool {
        self.to_server.is_empty()
            && self.to_client.is_empty()
            && self.pushing.is_none()
            && !self.reading
            && self.field.wanted().is_none()
            && !self.field.read_back()
            && !self.field.expiring(i64::MAX)
            && self.since == self.version
    }

    /// Lost requests leave the client waiting: as a timeout would, give up
    /// on them.
    fn time_out_lost(&mut self) {
        let epoch = self.epoch;
        if let Some(push) = self.pushing {
            let sent = self.to_server.iter().any(|s| {
                s.epoch == epoch && matches!(s.item, Req::Write { push: p, .. } if p == push)
            });
            let answered = self.to_client.iter().any(|s| {
                s.epoch == epoch && matches!(s.item, Answer::Write { push: p, .. } if p == push)
            });
            if !sent && !answered {
                self.pushing = None;
                self.given_up.push((push, self.ms));
                self.entry();
                {
                    trace!(self, "gave up");
                    self.field.ended(Ended::Unknown { at_ms: self.ms }).unwrap();
                }
            }
        }
        let current = |s: &&Sent<Req>| s.epoch == epoch;
        if self.reading {
            let sent = self
                .to_server
                .iter()
                .filter(current)
                .any(|s| matches!(s.item, Req::Read { .. }));
            let answered = self
                .to_client
                .iter()
                .any(|s| s.epoch == epoch && matches!(s.item, Answer::Read { .. }));
            self.reading = sent || answered;
        }
        if self.syncing {
            let sent = self
                .to_server
                .iter()
                .chain(&self.parked)
                .filter(current)
                .any(|s| matches!(s.item, Req::Sync { .. }));
            let answered = self
                .to_client
                .iter()
                .any(|s| s.epoch == epoch && matches!(s.item, Answer::Sync { .. }));
            self.syncing = sent || answered;
        }
    }
}

/// Runs a seed; returns whether the user's intent was checked (else only
/// convergence).
fn run(seed: u64, provider: Provider) -> bool {
    let mut w = World::new(seed, provider);
    for _ in 0..400 {
        w.step(true);
    }
    let mut steps = 0;
    while !w.quiet() {
        steps += 1;
        assert!(
            steps < 20_000,
            "seed {seed}: no convergence: {:#?}",
            w.field
        );
        w.time_out_lost();
        w.step(false);
    }
    let f = &w.field;
    let context = || {
        format!(
            "seed {seed}, provider {provider:?}, server {}: {f:#?}",
            w.value
        )
    };
    match &f.decision {
        None => {
            assert!(!f.pending(), "{}", context());
            assert_eq!((f.desired, f.observed), (w.value, w.value), "{}", context());
        }
        Some(_) => assert!(f.pending(), "{}", context()),
    }
    // The user's last intent wins, unless another client wrote since.
    let (Some(want), false) = (w.last_want, w.other_since_want) else {
        return false;
    };
    // Had already (another client's), nothing is pushed.
    let expected = if want == w.value {
        None
    } else if provider.refused == Some(want) {
        Some(Decision::Refused("refused".into()))
    } else if provider.not_kept == Some(want) {
        Some(Decision::NotKept)
    } else {
        None
    };
    assert_eq!(f.decision, expected, "{}", context());
    if expected.is_none() {
        assert_eq!(w.value, want, "{}", context());
    }
    true
}

/// Runs `seeds`, of which at least `share` must check the user's intent.
fn sweep(seeds: std::ops::Range<u64>, provider: Provider, share: f64) {
    let total = seeds.end - seeds.start;
    let checked = seeds.filter(|&seed| run(seed, provider)).count() as u64;
    let msg = format!("{checked} of {total} runs checked the user's intent, {provider:?}");
    eprintln!("{msg}");
    assert!(checked as f64 >= share * total as f64, "{msg}");
}

const SEEDS: std::ops::Range<u64> = 0..2000;

#[test]
fn every_interleaving_converges_to_the_users_intent() {
    let provider = Provider {
        not_kept: None,
        refused: None,
        others: false,
    };
    sweep(SEEDS, provider, 1.0);
}

#[test]
fn values_not_kept_or_refused_need_a_decision() {
    let provider = Provider {
        not_kept: Some(3),
        refused: Some(2),
        others: false,
    };
    sweep(SEEDS, provider, 1.0);
}

#[test]
fn another_clients_changes_converge() {
    for (not_kept, refused) in [(None, None), (Some(3), Some(2))] {
        let provider = Provider {
            not_kept,
            refused,
            others: true,
        };
        sweep(SEEDS, provider, 0.5);
    }
}
