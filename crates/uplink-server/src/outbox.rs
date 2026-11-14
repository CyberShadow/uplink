//! The outbox (SPEC §6.6): what of every account, mail or Matrix, is not
//! yet reconciled with its provider. Each domain lists its own accounts;
//! this lists them together, in one read, so as of one log position.

use uplink_protocol::ServerResponse;
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::answer::{internal, Reader, Response};

#[derive(Clone)]
pub struct Outbox {
    reader: Reader,
}

impl Outbox {
    /// `reader` is a read-only connection to the database.
    pub fn new(reader: Connection) -> Self {
        Self {
            reader: Reader::new(reader),
        }
    }

    /// The accounts with anything not yet reconciled, in id order.
    pub async fn list(&self) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
                let mut accounts = uplink_email::outbox::outbox(&tx).map_err(internal)?;
                accounts.extend(uplink_matrix::outbox::outbox(&tx).map_err(internal)?);
                accounts.extend(uplink_calendar::respond::outbox(&tx).map_err(internal)?);
                accounts.sort_by(|a, b| a.account.cmp(&b.account));
                Ok(ServerResponse::Outbox { as_of, accounts })
            })
            .await
    }
}
