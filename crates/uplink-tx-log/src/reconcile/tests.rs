use super::*;

fn id(n: i64) -> TxLogId {
    TxLogId(n)
}

/// A field, its position counted as the log's would be.
struct Log {
    field: Field<u8>,
    at: i64,
}

impl Log {
    fn new(value: u8) -> Self {
        Self {
            field: Field::new(value, id(0)),
            at: 1,
        }
    }

    /// The position a request sent now takes as `as_of`.
    fn now(&self) -> TxLogId {
        id(self.at)
    }

    /// A new entry.
    fn entry(&mut self) -> TxLogId {
        self.at += 1;
        id(self.at)
    }

    fn want(&mut self, value: u8) -> TxLogId {
        let by = self.entry();
        self.field.want(value, by);
        by
    }

    fn claim(&mut self) -> u8 {
        self.entry();
        self.field.claim().unwrap()
    }

    fn applied(&mut self) {
        let at = self.entry();
        self.field.ended(Ended::Applied, at).unwrap();
    }

    fn ended(&mut self, ended: Ended) {
        let at = self.entry();
        self.field.ended(ended, at).unwrap();
    }

    /// Wanted 2 and pushed it, superseding the unknown push, but refused.
    fn ended_refused_later(&mut self) {
        self.want(2);
        self.claim();
        self.ended(Ended::NotApplied {
            refused: Some("no".into()),
        });
        assert_eq!(self.field.wanted(), None, "to decide");
        assert!(!self.field.read_back(), "the unknown push unresolved");
    }

    fn observe(&mut self, value: u8, as_of: TxLogId) {
        let at = self.entry();
        self.field.observe(value, as_of, at).unwrap();
    }
}

#[test]
fn a_change_is_pushed_and_confirmed_then_done() {
    let mut f = Log::new(0);
    f.want(1);
    assert!(f.field.pending());
    assert_eq!(f.claim(), 1);
    assert_eq!(f.field.wanted(), None, "in flight");
    f.applied();
    assert!(f.field.pending(), "until confirmed");
    assert!(f.field.read_back());
    let as_of = f.now();
    f.observe(1, as_of);
    assert!(!f.field.pending());
    assert_eq!((f.field.desired, f.field.observed), (1, 1));
}

#[test]
fn a_change_and_its_undo_before_a_push_push_nothing() {
    let mut f = Log::new(0);
    f.want(1);
    f.want(0);
    assert!(!f.field.pending());
    assert_eq!(f.field.wanted(), None);
}

/// The CI failure: a sync answered before the push, recorded after its
/// outcome, is not taken for the provider's state.
#[test]
fn a_sync_older_than_a_push_does_not_overwrite_it() {
    let mut f = Log::new(0);
    let stale = f.now();
    f.want(1);
    f.claim();
    f.applied();
    f.observe(0, stale);
    assert_eq!(f.field.desired, 1);
    assert!(f.field.pending());
    // It contradicts: read back, to tell whether it was stale.
    assert!(f.field.read_back());
}

/// Undoing a push applied but not yet confirmed pushes the undo, though it
/// asks for what was last observed.
#[test]
fn undoing_before_the_echo_is_pushed() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.applied();
    f.want(0);
    assert_eq!(f.field.wanted(), Some(&0));
    f.claim();
    f.applied();
    // The first push's echo, late: older than the undo's outcome. It may
    // be another's change since; the undo's push no longer awaited, it is
    // read back.
    let as_of = id(f.at - 2);
    f.observe(1, as_of);
    assert!(f.field.read_back());
    assert_eq!(f.field.desired, 0);
    let as_of = f.now();
    f.observe(0, as_of);
    assert!(!f.field.pending());
    assert!(!f.field.read_back());
}

#[test]
fn a_push_of_unknown_outcome_expires_so_that_reads_settle_it() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.ended(Ended::Unknown { at_ms: 1_000 });
    f.ended_refused_later();
    assert!(!f.field.expiring(1_000 + UNKNOWN_FOR_MS - 1));
    assert!(f.field.expiring(1_000 + UNKNOWN_FOR_MS));
    let at = f.entry();
    f.field.expire(1_000 + UNKNOWN_FOR_MS, at).unwrap();
    assert!(f.field.read_back(), "what became of it");
    let as_of = f.now();
    f.observe(2, as_of);
    assert!(!f.field.pending(), "the provider had what was wanted");
}

/// Undoing while a push is in flight: its outcome, then the undo, pushed.
#[test]
fn undoing_while_in_flight_waits_for_the_outcome() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.want(0);
    assert!(f.field.pending(), "the push may land");
    assert_eq!(f.field.wanted(), None, "one push at a time");
    f.applied();
    assert_eq!(f.field.wanted(), Some(&0));
}

/// A stale sync agreeing with an undo confirms nothing (A→B→A).
#[test]
fn a_stale_sync_agreeing_with_an_undo_confirms_nothing() {
    let mut f = Log::new(0);
    let stale = f.now();
    f.want(1);
    f.claim();
    f.applied();
    f.want(0);
    f.claim();
    f.ended(Ended::Unknown { at_ms: 0 });
    f.observe(0, stale);
    assert!(f.field.pending());
    assert_eq!(f.field.wanted(), Some(&0), "the unknown push again");
}

#[test]
fn a_push_not_applied_ends_as_if_not_made() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.want(0);
    f.ended(Ended::NotApplied { refused: None });
    assert!(!f.field.pending(), "the undo is fulfilled locally");
}

#[test]
fn a_push_not_applied_restores_the_unknown_one_it_superseded() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.ended(Ended::Unknown { at_ms: 0 });
    f.want(0);
    f.claim();
    f.ended(Ended::NotApplied { refused: None });
    let outcome = f.field.attempt.as_ref().unwrap().outcome;
    assert!(matches!(outcome, Outcome::Unknown { .. }), "{outcome:?}");
    assert_eq!(f.field.wanted(), Some(&0));
}

#[test]
fn refusals_wait_for_a_decision() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.ended(Ended::NotApplied {
        refused: Some("M_FORBIDDEN".into()),
    });
    assert_eq!(
        f.field.decision,
        Some(Decision::Refused("M_FORBIDDEN".into()))
    );
    assert_eq!(f.field.wanted(), None);
    f.want(0);
    assert!(!f.field.pending(), "decided: as the provider has it");
}

#[test]
fn a_value_not_kept_twice_needs_a_decision() {
    let mut f = Log::new(0);
    f.want(3);
    for round in 0..2 {
        assert_eq!(f.claim(), 3, "round {round}");
        f.applied();
        let as_of = f.now();
        f.observe(0, as_of);
    }
    assert_eq!(f.field.decision, Some(Decision::NotKept));
    assert_eq!(f.field.wanted(), None);
}

#[test]
fn of_two_observations_sent_together_neither_is_trusted_over_the_other() {
    let mut f = Log::new(0);
    let as_of = f.now();
    f.observe(1, as_of);
    assert_eq!(f.field.desired, 1);
    // Sent at the same position, it may be the older.
    f.observe(0, as_of);
    assert_eq!(f.field.desired, 1);
    assert!(f.field.read_back());
    let as_of = f.now();
    f.observe(0, as_of);
    assert_eq!(f.field.desired, 0);
    assert!(!f.field.read_back());
}

/// A push given up on, superseded by one applied and confirmed, may still
/// land: the opinion waits for it to expire, then reads back.
#[test]
fn a_superseded_push_of_unknown_outcome_may_still_land() {
    let mut f = Log::new(0);
    f.want(1);
    f.claim();
    f.ended(Ended::Unknown { at_ms: 0 });
    f.want(2);
    f.claim();
    f.applied();
    let as_of = f.now();
    f.observe(2, as_of);
    assert!(f.field.pending(), "the push of 1 may land");
    assert_eq!(f.field.wanted(), None);
    // It lands: pushed again.
    let as_of = f.now();
    f.observe(1, as_of);
    assert_eq!(f.field.wanted(), Some(&2));
    f.claim();
    f.applied();
    let as_of = f.now();
    f.observe(2, as_of);
    assert!(f.field.pending());
    let at = f.entry();
    f.field.expire(UNKNOWN_FOR_MS, at).unwrap();
    assert!(f.field.read_back());
    let as_of = f.now();
    f.observe(2, as_of);
    assert!(!f.field.pending());
    assert!(!f.field.read_back());
}

mod simulation;
