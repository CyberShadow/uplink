//! The work SQLite does, counted: steps of its virtual machine, which
//! are as many each time the same statements run over the same data,
//! however busy the machine, unlike the time they take. So tests can
//! tell how the work of something grows with the database (SPEC §4.2).

use rusqlite::Connection;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// The steps run on the connections it counts on.
#[derive(Clone, Default)]
pub struct Work(Arc<AtomicU64>);

impl Work {
    /// Counts the steps run on `conn` from now on (in place of any
    /// other progress handler of it).
    pub fn count_on(&self, conn: &Connection) {
        let steps = self.0.clone();
        conn.progress_handler(
            1,
            Some(move || {
                steps.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("a connection of this thread");
    }

    /// The steps run so far.
    pub fn steps(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// The steps `f` runs, and what it returns.
    pub fn of<T>(&self, f: impl FnOnce() -> T) -> (u64, T) {
        let before = self.steps();
        let returned = f();
        (self.steps() - before, returned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_grow_with_the_rows_a_statement_goes_through() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (x INTEGER PRIMARY KEY);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 4000)
             INSERT INTO t SELECT x FROM n;",
        )
        .unwrap();
        let work = Work::default();
        work.count_on(&conn);
        let scan = |below: i64| {
            work.of(|| {
                conn.query_row("SELECT count(*) FROM t WHERE x + 0 < ?1", [below], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
            })
        };
        let looked_up = work
            .of(|| {
                conn.query_row("SELECT x FROM t WHERE x = 3", [], |r| r.get::<_, i64>(0))
                    .unwrap()
            })
            .0;
        // A scan steps through every row, a look-up through one.
        let (all, _) = scan(0);
        assert!(all > 4000, "{all}");
        assert!(looked_up < 100, "{looked_up}");
        // And as often as run, the same.
        assert_eq!(scan(0).0, all);
    }
}
