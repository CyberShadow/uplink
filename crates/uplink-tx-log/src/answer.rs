//! Answering clients' requests (SPEC §5.4): ops through the dispatcher,
//! queries on reader connections.

use crate::{Conflict, Dispatcher, Op};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use uplink_protocol::{ErrorCode, ServerError, ServerResponse};
use uplink_storage::rusqlite::Connection;

pub use uplink_realtime::Response;

pub fn error(code: ErrorCode, message: String) -> ServerError {
    ServerError { code, message }
}

/// A failure that is uplink's, not the request's; logged.
pub fn internal(e: anyhow::Error) -> ServerError {
    tracing::error!("request failed: {e:#}");
    error(ErrorCode::Internal, format!("{e:#}"))
}

impl Dispatcher {
    /// Dispatches `op`, answering with its entry; a [`Conflict`] is the
    /// request's refusal.
    pub async fn answer(&self, op: impl Op) -> Response {
        let tx_log_id = self
            .dispatch(op)
            .await
            .map_err(|e| match e.downcast::<Conflict>() {
                Ok(conflict) => error(ErrorCode::Conflict, conflict.0),
                Err(e) => internal(e),
            })?;
        Ok(ServerResponse::Dispatched { tx_log_id })
    }
}

/// Read-only connections to the database the dispatcher writes, shared
/// by a domain's queries: each runs on one of them, and waits only while
/// all are busy. (SQLite's WAL lets them read at once, as the writer
/// writes.)
#[derive(Clone)]
pub struct Reader {
    /// Those not running a query.
    idle: Arc<Mutex<Vec<Connection>>>,
    /// As many permits as are idle, or about to be.
    permits: Arc<Semaphore>,
}

impl Reader {
    /// Of one connection: its queries run one at a time.
    pub fn new(conn: Connection) -> Self {
        Self::pool(vec![conn])
    }

    /// Of `conns`, of which there is at least one.
    pub fn pool(conns: Vec<Connection>) -> Self {
        assert!(!conns.is_empty(), "a reader reads on some connection");
        Self {
            permits: Arc::new(Semaphore::new(conns.len())),
            idle: Arc::new(Mutex::new(conns)),
        }
    }

    /// Runs `f` on an idle connection, off the async runtime.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, ServerError> + Send + 'static,
    ) -> Result<R, ServerError> {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let idle = self.idle.clone();
        let task = move || {
            // Dropped after `lent`: a permit is of a connection idle.
            let _permit = permit;
            let conn = idle.lock().expect("idle poisoned").pop();
            let mut lent = Lent {
                conn: Some(conn.expect("a permit is of an idle connection")),
                idle,
            };
            f(lent.conn.as_mut().expect("lent until dropped"))
        };
        match tokio::task::spawn_blocking(task).await {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

/// A connection taken from the idle, given back as dropped, though the
/// query on it panics.
struct Lent {
    conn: Option<Connection>,
    idle: Arc<Mutex<Vec<Connection>>>,
}

impl Drop for Lent {
    fn drop(&mut self) {
        let conn = self.conn.take().expect("given back once");
        // Poisoned only by a panic while it was held, which this outlives.
        match self.idle.lock() {
            Ok(mut idle) => idle.push(conn),
            Err(mut poisoned) => poisoned.get_mut().push(conn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_at_once_on_a_pool() {
        let reader = Reader::pool(vec![
            Connection::open_in_memory().unwrap(),
            Connection::open_in_memory().unwrap(),
        ]);
        // Each waits for the other: they finish only if they run at once.
        let both = Arc::new(Barrier::new(2));
        let read = |both: Arc<Barrier>| {
            reader.read(move |_| {
                both.wait();
                Ok(())
            })
        };
        let (a, b) = tokio::join!(read(both.clone()), read(both));
        a.unwrap();
        b.unwrap();
    }

    #[tokio::test]
    async fn a_connection_outlives_a_query_panicking_on_it() {
        let reader = Reader::new(Connection::open_in_memory().unwrap());
        let panicked = tokio::spawn({
            let reader = reader.clone();
            async move {
                reader
                    .read(|_| -> Result<(), ServerError> { panic!("query") })
                    .await
            }
        })
        .await;
        assert!(panicked.unwrap_err().is_panic());
        let one: i64 = reader
            .read(|conn| Ok(conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap()))
            .await
            .unwrap();
        assert_eq!(one, 1);
    }
}
