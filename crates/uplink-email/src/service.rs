//! Answers the mail requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    compose::{self, CancelSend, DeleteDraft, EditDraft, PutDraft, SendDraft},
    mailbox::{LabelChange, SetLabels},
    parse::parse,
    query::{self, ConfiguredAccount},
    store::{MailStore, VariantHash},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use uplink_protocol::{
    compose::{DraftContent, DraftId, OutboxId, Splice},
    mail::{EntryId, LabelId, ListPosition, MessageContent, MessageId},
    ErrorCode, ServerError, ServerResponse,
};
use uplink_storage::rusqlite::Connection;
use uplink_tx_log::{Conflict, Dispatcher, Op};

/// The most entries a list or search request may ask for.
pub const MAX_LIMIT: u32 = 500;

#[derive(Clone)]
pub struct Mail {
    dispatcher: Dispatcher,
    reader: Arc<Mutex<Connection>>,
    store: MailStore,
    accounts: Arc<Vec<ConfiguredAccount>>,
    send_delay: Duration,
}

type Response = Result<ServerResponse, ServerError>;

impl Mail {
    /// `reader` is a read-only connection to the database `dispatcher`
    /// writes.
    pub fn new(
        dispatcher: Dispatcher,
        reader: Connection,
        data_root: &Path,
        accounts: Vec<ConfiguredAccount>,
        send_delay: Duration,
    ) -> Self {
        Self {
            dispatcher,
            reader: Arc::new(Mutex::new(reader)),
            store: MailStore::new(data_root),
            accounts: Arc::new(accounts),
            send_delay,
        }
    }

    pub async fn list_accounts(&self) -> Response {
        let accounts = self.accounts.clone();
        self.read(move |conn, _| {
            let (as_of, accounts) = query::accounts(conn, &accounts).map_err(internal)?;
            Ok(ServerResponse::Accounts { as_of, accounts })
        })
        .await
    }

    pub async fn list_entries(
        &self,
        label: LabelId,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Response {
        check_limit(limit)?;
        self.read(move |conn, _| {
            if query::label_accounts(conn, &[label]).map_err(internal)?[0].is_none() {
                return Err(error(ErrorCode::NotFound, format!("no label {}", label.0)));
            }
            let page = query::entries(conn, label, after, limit).map_err(internal)?;
            Ok(ServerResponse::Entries {
                as_of: page.as_of,
                entries: page.entries,
                next: page.next,
            })
        })
        .await
    }

    pub async fn search(&self, query: String, limit: u32) -> Response {
        check_limit(limit)?;
        if query.split_whitespace().next().is_none() {
            return Err(error(ErrorCode::Invalid, "empty search query".into()));
        }
        self.read(move |conn, _| {
            let (as_of, entries) = query::search(conn, &query, limit).map_err(internal)?;
            Ok(ServerResponse::Entries {
                as_of,
                entries,
                next: None,
            })
        })
        .await
    }

    pub async fn load_message(&self, message: MessageId) -> Response {
        self.read(move |conn, store| {
            let (message_id, variant) = query::preferred_variant(conn, message)
                .map_err(internal)?
                .ok_or_else(|| error(ErrorCode::NotFound, format!("no message {}", message.0)))?;
            let raw = store.get(&VariantHash(variant)).map_err(internal)?;
            let parsed = parse(&raw);
            Ok(ServerResponse::Message {
                message: MessageContent {
                    id: message,
                    message_id,
                    subject: parsed.subject,
                    from_addr: parsed.from_addr,
                    from_name: parsed.from_name,
                    to_addrs: parsed.to_addrs,
                    cc_addrs: parsed.cc_addrs,
                    date_sent: parsed.date_sent,
                    body_text: parsed.body_text,
                },
            })
        })
        .await
    }

    pub async fn modify_labels(
        &self,
        entries: Vec<EntryId>,
        add: Vec<LabelId>,
        remove: Vec<LabelId>,
    ) -> Response {
        if add.iter().any(|l| remove.contains(l)) {
            return Err(error(
                ErrorCode::Invalid,
                "a label is both added and removed".into(),
            ));
        }
        let (entries, add, remove) = self
            .read(move |conn, _| {
                validate_modify_labels(conn, &entries, &add, &remove)?;
                Ok((entries, add, remove))
            })
            .await?;
        let changes = entries
            .iter()
            .flat_map(|&entry| {
                let added = add.iter().map(move |&label| LabelChange {
                    entry,
                    label,
                    desired: true,
                });
                let removed = remove.iter().map(move |&label| LabelChange {
                    entry,
                    label,
                    desired: false,
                });
                added.chain(removed)
            })
            .collect();
        self.dispatch(SetLabels { changes }).await
    }

    pub async fn list_drafts(&self) -> Response {
        self.read(move |conn, _| {
            let tx = conn.transaction().map_err(|e| internal(e.into()))?;
            let as_of = query::as_of(&tx).map_err(internal)?;
            let drafts = compose::drafts(&tx).map_err(internal)?;
            Ok(ServerResponse::Drafts { as_of, drafts })
        })
        .await
    }

    pub async fn create_draft(
        &self,
        id: DraftId,
        account: String,
        content: DraftContent,
        in_reply_to: Option<MessageId>,
    ) -> Response {
        if !self.accounts.iter().any(|a| a.id == account) {
            return Err(error(ErrorCode::NotFound, format!("no account {account}")));
        }
        if let Some(message) = in_reply_to {
            self.read(move |conn, _| {
                query::preferred_variant(conn, message)
                    .map_err(internal)?
                    .map(drop)
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no message {}", message.0)))
            })
            .await?;
        }
        self.dispatch(PutDraft {
            id,
            account,
            revision: 0,
            content,
            in_reply_to,
        })
        .await
    }

    pub async fn edit_draft(&self, draft: DraftId, revision: i64, splice: Splice) -> Response {
        self.dispatch(EditDraft {
            id: draft,
            revision: Some(revision),
            splice,
        })
        .await
    }

    pub async fn delete_draft(&self, draft: DraftId) -> Response {
        self.dispatch(DeleteDraft { id: draft }).await
    }

    /// Refuses drafts that could not be sent. A draft edited meanwhile in
    /// another tab may still turn out unsendable; it then fails in the
    /// outbox.
    pub async fn send_draft(&self, draft: DraftId) -> Response {
        let id = draft.clone();
        self.read(move |conn, _| {
            let tx = conn.transaction().map_err(|e| internal(e.into()))?;
            let draft = compose::drafts(&tx)
                .map_err(internal)?
                .into_iter()
                .find(|d| d.id == id)
                .ok_or_else(|| error(ErrorCode::NotFound, format!("no draft {}", id.0)))?;
            compose::check_sendable(&draft.content)
                .map_err(|e| error(ErrorCode::Invalid, format!("{e:#}")))
        })
        .await?;
        self.dispatch(SendDraft {
            id: draft,
            delay_ms: self.send_delay.as_millis().try_into().expect("delay fits"),
        })
        .await
    }

    pub async fn cancel_send(&self, item: OutboxId) -> Response {
        self.dispatch(CancelSend { item }).await
    }

    pub async fn list_outbox(&self) -> Response {
        self.read(move |conn, _| {
            let tx = conn.transaction().map_err(|e| internal(e.into()))?;
            let as_of = query::as_of(&tx).map_err(internal)?;
            let items = compose::outbox(&tx).map_err(internal)?;
            Ok(ServerResponse::Outbox { as_of, items })
        })
        .await
    }

    async fn dispatch(&self, op: impl Op) -> Response {
        let tx_log_id =
            self.dispatcher
                .dispatch(op)
                .await
                .map_err(|e| match e.downcast::<Conflict>() {
                    Ok(conflict) => error(ErrorCode::Conflict, conflict.0),
                    Err(e) => internal(e),
                })?;
        Ok(ServerResponse::Dispatched { tx_log_id })
    }

    async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection, &MailStore) -> Result<R, ServerError> + Send + 'static,
    ) -> Result<R, ServerError> {
        let reader = self.reader.clone();
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            f(&mut reader.lock().expect("reader poisoned"), &store)
        })
        .await
        {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

/// The labels must exist and be of one account, which the entries must be
/// of too.
fn validate_modify_labels(
    conn: &Connection,
    entries: &[EntryId],
    add: &[LabelId],
    remove: &[LabelId],
) -> Result<(), ServerError> {
    let labels: Vec<LabelId> = add.iter().chain(remove).copied().collect();
    let label_accounts = query::label_accounts(conn, &labels).map_err(internal)?;
    let mut account = None;
    for (label, label_account) in labels.iter().zip(label_accounts) {
        let label_account = label_account
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no label {}", label.0)))?;
        if *account.get_or_insert_with(|| label_account.clone()) != label_account {
            return Err(error(
                ErrorCode::Invalid,
                "labels of different accounts".into(),
            ));
        }
    }
    let entry_accounts = query::entry_accounts(conn, entries).map_err(internal)?;
    for (entry, entry_account) in entries.iter().zip(entry_accounts) {
        let entry_account = entry_account
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no entry {}", entry.0)))?;
        if account.as_ref().is_some_and(|a| *a != entry_account) {
            return Err(error(
                ErrorCode::Invalid,
                format!("entry {} is not of the labels' account", entry.0),
            ));
        }
    }
    Ok(())
}

fn check_limit(limit: u32) -> Result<(), ServerError> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(error(
            ErrorCode::Invalid,
            format!("limit must be between 1 and {MAX_LIMIT}"),
        ))
    }
}

fn error(code: ErrorCode, message: String) -> ServerError {
    ServerError { code, message }
}

fn internal(e: anyhow::Error) -> ServerError {
    tracing::error!("request failed: {e:#}");
    error(ErrorCode::Internal, format!("{e:#}"))
}
