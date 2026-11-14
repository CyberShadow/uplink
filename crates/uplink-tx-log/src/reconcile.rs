//! The reconciliation of one field (SPEC §6.6): a value the provider holds,
//! which uplink mirrors as desired (set by ops) and observed (seen of the
//! provider), with the attempt, if any, to bring the provider in line.
//!
//! Pure: no database, no provider. A crate stores a [`Field`] per value it
//! mirrors, feeds it what happens (an op, a claim, a push's outcome, an
//! observation) and asks it what to do (push, read back). Positions are
//! those of the log: an observation's `as_of` is the last entry's id
//! before its request was sent; `at`, of an outcome or an observation,
//! the entry recording it. A request sent after an entry is answered after
//! what the entry recorded was: so, of observations, the one sent later
//! is not necessarily the newer (a long poll is answered late), but one
//! sent after another was recorded is.

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::TxLogId;

/// How long after a push's outcome became unknown the provider is taken
/// to be done with it: applied by then, or never to be (SPEC §6.6.3).
pub const UNKNOWN_FOR_MS: i64 = 5 * 60 * 1000;

/// One value of the provider's, as uplink mirrors it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field<V> {
    /// What the user wants, shown as such.
    pub desired: V,
    /// The op whose opinion `desired` is; `None`: no opinion, desired
    /// follows observed. Some whenever there is an attempt or a
    /// straggler, or desired differs from observed.
    pub opinion: Option<TxLogId>,
    /// The value last seen authoritatively.
    pub observed: V,
    /// Observations whose `as_of` is before this may predate what is
    /// known, so are not trusted: the entry recording an applied push's
    /// outcome, or the last observation trusted.
    pub known_at: TxLogId,
    /// The last push, until it is confirmed or superseded.
    pub attempt: Option<Attempt<V>>,
    /// Since when (Unix ms) the latest push of unknown outcome that no
    /// longer matters to `attempt` is so: until it expires, it may yet
    /// land and undo a later one, so the opinion is not done.
    pub straggler: Option<i64>,
    /// An observation contradicted what is known, but could not be
    /// trusted, as recorded at this entry: the field is to be read back by
    /// a request sent after it.
    pub check: Option<TxLogId>,
    /// How often an authoritative observation contradicted an applied
    /// push of the current opinion.
    pub not_kept: u8,
    /// Why the difference needs the user's decision, if it does.
    pub decision: Option<Decision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt<V> {
    pub value: V,
    /// The opinion it pushed.
    pub opinion: TxLogId,
    pub outcome: Outcome,
    /// The attempt it superseded, while its own outcome is unknown: what
    /// the provider has if this one is not applied.
    pub replaces: Option<Box<Attempt<V>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// Sent, not yet answered.
    InFlight,
    /// The provider accepted it, recorded at `at`; it has the value unless
    /// changed since.
    Applied { at: TxLogId },
    /// It may or may not have been applied, and may yet be; so since
    /// `since_ms` (Unix ms).
    Unknown { since_ms: i64 },
    /// Unknown for long enough that the provider is done with it, as
    /// recorded at `at`: applied or not, it will not be later.
    Expired { at: TxLogId },
}

impl Outcome {
    /// Whether observations can be ordered after it.
    fn ordered(self) -> bool {
        matches!(self, Self::Applied { .. } | Self::Expired { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    /// The provider refused it, saying why.
    Refused(String),
    /// The provider accepted it but kept another value, twice.
    NotKept,
}

impl Decision {
    /// Why, as the user is told.
    pub fn reason(&self) -> &str {
        match self {
            Self::Refused(reason) => reason,
            Self::NotKept => "the provider kept another value",
        }
    }
}

/// How a push ended, as the provider's crate classifies it (SPEC §6.6.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Ended {
    /// Accepted.
    Applied,
    /// The provider cannot have acted on it; refused if it said why it
    /// won't, else to be retried.
    NotApplied { refused: Option<String> },
    /// It may have acted on it, as of `at_ms` (Unix ms).
    Unknown { at_ms: i64 },
}

impl<V: Clone + PartialEq> Field<V> {
    /// A field first seen as `value`, as recorded at `at`, of which uplink
    /// has no opinion.
    pub fn new(value: V, at: TxLogId) -> Self {
        Self {
            desired: value.clone(),
            opinion: None,
            observed: value,
            known_at: at,
            attempt: None,
            straggler: None,
            check: None,
            not_kept: 0,
            decision: None,
        }
    }

    /// Whether uplink has an opinion the provider may not reflect.
    pub fn pending(&self) -> bool {
        self.opinion.is_some()
    }

    /// The value to push now, if any: unless one is in flight, or waits
    /// for confirmation of the value still desired, or the user is to
    /// decide.
    pub fn wanted(&self) -> Option<&V> {
        if self.opinion.is_none() || self.decision.is_some() {
            return None;
        }
        let push = match &self.attempt {
            None => self.desired != self.observed,
            Some(a) => match a.outcome {
                Outcome::InFlight => false,
                Outcome::Applied { .. } | Outcome::Expired { .. } => a.value != self.desired,
                Outcome::Unknown { .. } => true,
            },
        };
        push.then_some(&self.desired)
    }

    /// What the provider has, as far as uplink's own pushes tell: the
    /// value of its last push, else the value observed. Of a set the
    /// provider writes whole, each member's part in the next write (SPEC
    /// §6.6.4), so that a write never undoes uplink's own pushes.
    pub fn as_pushed(&self) -> &V {
        self.attempt.as_ref().map_or(&self.observed, |a| &a.value)
    }

    /// Whether to read the field back: an applied push of what is still
    /// desired waits for an observation after it; what became of one of
    /// unknown outcome, now expired, is to be learnt; or an untrusted
    /// observation contradicted what is known. Not while a push is in
    /// flight or of unknown outcome: no observation is trusted then, so
    /// reading would tell nothing.
    pub fn read_back(&self) -> bool {
        let check = self.check.is_some();
        match &self.attempt {
            None => check,
            Some(a) => match a.outcome {
                Outcome::Applied { .. } => check || a.value == self.desired,
                Outcome::Expired { .. } => true,
                Outcome::InFlight | Outcome::Unknown { .. } => false,
            },
        }
    }

    /// An op `by` asks for `value`.
    pub fn want(&mut self, value: V, by: TxLogId) {
        self.desired = value;
        self.opinion = Some(by);
        self.not_kept = 0;
        // The user has decided.
        self.decision = None;
        self.settle();
    }

    /// The reconciler claims the field's push, of [`Self::wanted`].
    pub fn claim(&mut self) -> Result<V> {
        ensure!(self.wanted().is_some(), "nothing to push");
        let opinion = self.opinion.expect("wanted has an opinion");
        self.attempt = Some(Attempt {
            value: self.desired.clone(),
            opinion,
            outcome: Outcome::InFlight,
            replaces: self.attempt.take().map(Box::new),
        });
        Ok(self.desired.clone())
    }

    /// The claimed push ended so, as recorded at `at`.
    pub fn ended(&mut self, ended: Ended, at: TxLogId) -> Result<()> {
        let attempt = self.attempt.as_mut();
        let Some(attempt) = attempt.filter(|a| a.outcome == Outcome::InFlight) else {
            bail!("no push in flight");
        };
        match ended {
            Ended::Applied => {
                ensure!(
                    at >= self.known_at,
                    "applied at {at:?}, before {:?}",
                    self.known_at
                );
                attempt.outcome = Outcome::Applied { at };
                let superseded = attempt.replaces.take();
                self.known_at = at;
                self.strand(superseded);
            }
            Ended::Unknown { at_ms } => {
                // Of what it superseded, it is the later, but they may
                // land in any order.
                attempt.outcome = Outcome::Unknown { since_ms: at_ms };
                let superseded = attempt.replaces.take();
                self.strand(superseded);
            }
            Ended::NotApplied { refused } => {
                let attempt = self.attempt.take().expect("matched above");
                // Still wanted, it needs a decision.
                if let Some(reason) = refused.filter(|_| attempt.value == self.desired) {
                    self.decision = Some(Decision::Refused(reason));
                }
                self.attempt = attempt.replaces.map(|a| *a);
            }
        }
        self.settle();
        Ok(())
    }

    /// The process stopped, at `at_ms`, with the push in flight: its
    /// outcome is unknown.
    pub fn interrupted(&mut self, at_ms: i64) {
        if let Some(a) = self
            .attempt
            .as_mut()
            .filter(|a| a.outcome == Outcome::InFlight)
        {
            a.outcome = Outcome::Unknown { since_ms: at_ms };
            let superseded = a.replaces.take();
            self.strand(superseded);
        }
    }

    /// Superseded pushes no longer tracked as what the provider has
    /// otherwise: those of unknown outcome become the straggler.
    fn strand(&mut self, mut superseded: Option<Box<Attempt<V>>>) {
        while let Some(a) = superseded {
            if let Outcome::Unknown { since_ms } = a.outcome {
                self.straggler = self.straggler.max(Some(since_ms));
            }
            superseded = a.replaces;
        }
    }

    /// Whether a push of unknown outcome has been so long enough, at
    /// `now_ms`, to [`Self::expire`].
    pub fn expiring(&self, now_ms: i64) -> bool {
        let due = |since_ms: i64| now_ms - since_ms >= UNKNOWN_FOR_MS;
        let attempt = self.attempt.as_ref().map(|a| a.outcome);
        self.straggler.is_some_and(due)
            || matches!(attempt, Some(Outcome::Unknown { since_ms }) if due(since_ms))
    }

    /// The provider is done, at `now_ms`, with the pushes of unknown
    /// outcome that have been so long enough, as recorded at `at`: what is
    /// observed after it can be trusted, and is to be.
    pub fn expire(&mut self, now_ms: i64, at: TxLogId) -> Result<()> {
        ensure!(
            self.expiring(now_ms),
            "no push of unknown outcome to expire"
        );
        ensure!(
            at >= self.known_at,
            "expired at {at:?}, before {:?}",
            self.known_at
        );
        let due = |since_ms: i64| now_ms - since_ms >= UNKNOWN_FOR_MS;
        if self.straggler.is_some_and(due) {
            self.straggler = None;
            // It may have landed, after what was observed.
            self.check = Some(at);
        }
        if let Some(a) = self.attempt.as_mut() {
            if matches!(a.outcome, Outcome::Unknown { since_ms } if due(since_ms)) {
                a.outcome = Outcome::Expired { at };
            }
        }
        self.known_at = at;
        Ok(())
    }

    /// The provider was seen to have `value`, by a request sent after
    /// `as_of`, as recorded at `at`.
    pub fn observe(&mut self, value: V, as_of: TxLogId, at: TxLogId) -> Result<()> {
        ensure!(
            as_of < at,
            "recorded at {at:?}, before sent after {as_of:?}"
        );
        let ordered = self.attempt.as_ref().is_none_or(|a| a.outcome.ordered());
        if ordered && as_of >= self.known_at {
            self.authoritative(value, as_of, at);
        } else {
            self.ambiguous(value, at);
        }
        self.settle();
        Ok(())
    }

    fn authoritative(&mut self, value: V, as_of: TxLogId, at: TxLogId) {
        self.observed = value;
        self.known_at = at;
        // A doubt raised by an observation answered after this one was
        // stands.
        if self.check.is_some_and(|c| as_of >= c) {
            self.check = None;
        }
        if let Some(attempt) = self.attempt.take() {
            let applied = matches!(attempt.outcome, Outcome::Applied { .. });
            // A straggler landing is no sign of the provider's.
            if applied
                && self.straggler.is_none()
                && Some(attempt.opinion) == self.opinion
                && attempt.value == self.desired
                && self.observed != attempt.value
            {
                self.not_kept += 1;
                if self.not_kept >= 2 {
                    self.decision = Some(Decision::NotKept);
                }
            }
        }
        if self.opinion.is_none() {
            self.desired = self.observed.clone();
        }
    }

    /// Stale, or another's change (even after a push, which may then not
    /// hold) that no sync will tell again: to be read back once that can
    /// be trusted, and then settled as any authoritative observation is
    /// (the local change winning, if there still is one). Until then the
    /// opinion and attempt stand: dropping either on the word of an
    /// untrusted observation could end an opinion on a stale `observed`.
    fn ambiguous(&mut self, value: V, at: TxLogId) {
        if value != self.observed || value != self.desired {
            self.check = Some(at);
        }
    }

    /// Without a push to await, an opinion the provider was seen to share
    /// is done.
    fn settle(&mut self) {
        if self.attempt.is_none() && self.straggler.is_none() && self.desired == self.observed {
            self.opinion = None;
            self.not_kept = 0;
            self.decision = None;
        }
    }
}

#[cfg(test)]
mod tests;
