//! Answering clients' requests (SPEC §5.4): ops through the dispatcher,
//! queries on a reader connection.

use crate::{Conflict, Dispatcher, Op};
use std::sync::{Arc, Mutex};
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

/// A read-only connection to the database the dispatcher writes, shared
/// by a domain's queries.
#[derive(Clone)]
pub struct Reader(Arc<Mutex<Connection>>);

impl Reader {
    pub fn new(conn: Connection) -> Self {
        Self(Arc::new(Mutex::new(conn)))
    }

    /// Runs `f` on the connection, off the async runtime.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, ServerError> + Send + 'static,
    ) -> Result<R, ServerError> {
        let conn = self.0.clone();
        match tokio::task::spawn_blocking(move || f(&mut conn.lock().expect("reader poisoned")))
            .await
        {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}
