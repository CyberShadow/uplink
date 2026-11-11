//! Answers the mail requests of the WebSocket protocol (SPEC §5.4).

use crate::{
    compose::{
        self, AttachToDraft, CancelSend, DeleteDraft, DetachFromDraft, EditDraft, KeepSent,
        PutDraft, SendDraft,
    },
    mailbox::{LabelChange, SetLabels},
    outbox,
    parse::{self, html_body, parse},
    query::{self, ConfiguredAccount},
    store::{Store, VariantHash},
};
use std::{path::Path, sync::Arc, time::Duration};
use uplink_protocol::{
    compose::{BlobId, DraftAttachmentId, DraftContent, DraftId, OutboxId, Splice, UploadedBlob},
    mail::{EntryId, EntryList, LabelId, ListPosition, MessageContent, MessageId},
    ErrorCode, ServerError, ServerResponse, TxLogId,
};
use uplink_storage::rusqlite::{Connection, OptionalExtension};
use uplink_tx_log::{
    answer::{error, internal, Reader, Response},
    Dispatcher,
};

/// The most entries a list or search request may ask for.
pub const MAX_LIMIT: u32 = 500;

/// The most a draft's attachments may total, in bytes: Gmail's limit.
/// Encoded as base64, they make a message of up to ~34 MB, within the 35
/// MB Gmail takes.
pub const MAX_ATTACHMENTS: u64 = 25 << 20;

#[derive(Clone)]
pub struct Mail {
    dispatcher: Dispatcher,
    reader: Reader,
    store: Store,
    blobs: Store,
    accounts: Arc<Vec<ConfiguredAccount>>,
    send_delay: Duration,
}

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
            reader: Reader::new(reader),
            store: Store::mail(data_root),
            blobs: Store::blobs(data_root),
            accounts: Arc::new(accounts),
            send_delay,
        }
    }

    pub async fn list_accounts(&self) -> Response {
        let accounts = self.accounts.clone();
        self.reader
            .read(move |conn| {
                let (as_of, accounts) = query::accounts(conn, &accounts).map_err(internal)?;
                Ok(ServerResponse::Accounts { as_of, accounts })
            })
            .await
    }

    pub async fn list_entries(
        &self,
        list: EntryList,
        after: Option<ListPosition>,
        limit: u32,
    ) -> Response {
        check_limit(limit)?;
        if let EntryList::AllMail { account } = &list {
            if !self.accounts.iter().any(|a| a.id == *account) {
                return Err(error(ErrorCode::NotFound, format!("no account {account}")));
            }
        }
        self.reader
            .read(move |conn| {
                if let EntryList::Label { label } = list {
                    if query::label_accounts(conn, &[label]).map_err(internal)?[0].is_none() {
                        return Err(error(ErrorCode::NotFound, format!("no label {}", label.0)));
                    }
                }
                let page = query::conversations(conn, &list, after, limit).map_err(internal)?;
                Ok(ServerResponse::Conversations {
                    as_of: page.as_of,
                    conversations: page.conversations,
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
        self.reader
            .read(move |conn| {
                let (as_of, conversations) =
                    query::search(conn, &query, limit).map_err(internal)?;
                Ok(ServerResponse::Conversations {
                    as_of,
                    conversations,
                    next: None,
                })
            })
            .await
    }

    pub async fn list_message_entries(&self, message: MessageId) -> Response {
        self.reader
            .read(move |conn| {
                let (as_of, entries) = query::message_entries(conn, message)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        error(ErrorCode::NotFound, format!("no message {}", message.0))
                    })?;
                Ok(ServerResponse::Entries { as_of, entries })
            })
            .await
    }

    pub async fn list_conversation(&self, entry: EntryId) -> Response {
        self.reader
            .read(move |conn| {
                let (as_of, entries) = query::conversation(conn, entry)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no entry {}", entry.0)))?;
                Ok(ServerResponse::Entries { as_of, entries })
            })
            .await
    }

    pub async fn load_message(&self, message: MessageId) -> Response {
        let store = self.store.clone();
        self.reader
            .read(move |conn| {
                let (message_id, variant) = query::preferred_variant(conn, message)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        error(ErrorCode::NotFound, format!("no message {}", message.0))
                    })?;
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
                        body_html: html_body(&raw),
                        attachments: parse::attachments(&raw),
                    },
                })
            })
            .await
    }

    /// The entry's message as its account has it, raw: the copy fetched
    /// from that account, which another account's may differ from.
    pub async fn raw_entry(&self, entry: EntryId) -> Result<Vec<u8>, ServerError> {
        let store = self.store.clone();
        self.reader
            .read(move |conn| {
                let variant: String = conn
                    .query_row(
                        "SELECT variant FROM mail_entry WHERE id = ?1",
                        [entry.0],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| internal(e.into()))?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no entry {}", entry.0)))?;
                store.get(&VariantHash(variant)).map_err(internal)
            })
            .await
    }

    /// The message's attachment at `index`: its name, MIME type and
    /// content.
    pub async fn attachment(
        &self,
        message: MessageId,
        index: u32,
    ) -> Result<(Option<String>, String, Vec<u8>), ServerError> {
        let store = self.store.clone();
        self.reader
            .read(move |conn| {
                let (_, variant) = query::preferred_variant(conn, message)
                    .map_err(internal)?
                    .ok_or_else(|| {
                        error(ErrorCode::NotFound, format!("no message {}", message.0))
                    })?;
                let raw = store.get(&VariantHash(variant)).map_err(internal)?;
                parse::attachment(&raw, index).ok_or_else(|| {
                    error(
                        ErrorCode::NotFound,
                        format!("message {} has no attachment {index}", message.0),
                    )
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
            .reader
            .read(move |conn| {
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
        self.dispatcher.answer(SetLabels { changes }).await
    }

    pub async fn list_drafts(&self) -> Response {
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let as_of = uplink_tx_log::as_of(&tx).map_err(internal)?;
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
            self.reader
                .read(move |conn| {
                    query::preferred_variant(conn, message)
                        .map_err(internal)?
                        .map(drop)
                        .ok_or_else(|| {
                            error(ErrorCode::NotFound, format!("no message {}", message.0))
                        })
                })
                .await?;
        }
        self.dispatcher
            .answer(PutDraft {
                id,
                account,
                revision: 0,
                content,
                attachments: vec![],
                in_reply_to,
            })
            .await
    }

    pub async fn edit_draft(&self, draft: DraftId, revision: i64, splice: Splice) -> Response {
        self.dispatcher
            .answer(EditDraft {
                id: draft,
                revision: Some(revision),
                splice,
            })
            .await
    }

    /// Stores an uploaded file, to be attached to drafts.
    pub async fn put_blob(&self, content: Vec<u8>) -> Result<UploadedBlob, ServerError> {
        let blobs = self.blobs.clone();
        tokio::task::spawn_blocking(move || {
            let blob = blobs.put(&content).map_err(internal)?;
            Ok(UploadedBlob {
                blob: BlobId(blob.0),
                size: content.len().try_into().expect("sizes fit in i64"),
            })
        })
        .await
        .expect("storing does not panic")
    }

    /// Stores the message's attachment at `index` as a blob, to attach it
    /// to a draft forwarding the message.
    pub async fn copy_attachment(
        &self,
        message: MessageId,
        index: u32,
    ) -> Result<UploadedBlob, ServerError> {
        let (_, _, content) = self.attachment(message, index).await?;
        self.put_blob(content).await
    }

    pub async fn attach_to_draft(
        &self,
        draft: DraftId,
        blob: BlobId,
        name: String,
        mime: String,
    ) -> Response {
        if name.trim().is_empty() || name.contains(char::is_control) {
            return Err(error(
                ErrorCode::Invalid,
                format!("not a file name: {name:?}"),
            ));
        }
        if !is_mime_type(&mime) {
            return Err(error(
                ErrorCode::Invalid,
                format!("not a MIME type: {mime:?}"),
            ));
        }
        let hash = VariantHash::parse(&blob.0)
            .ok_or_else(|| error(ErrorCode::Invalid, format!("not a blob: {:?}", blob.0)))?;
        let size = self
            .blobs
            .size(&hash)
            .map_err(internal)?
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no blob {}", blob.0)))?;
        let id = draft.clone();
        self.reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                let draft = compose::draft(&tx, &id)
                    .map_err(internal)?
                    .ok_or_else(|| error(ErrorCode::NotFound, format!("no draft {}", id.0)))?;
                let attached: i64 = draft.attachments.iter().map(|a| a.size).sum();
                if attached as u64 + size > MAX_ATTACHMENTS {
                    return Err(error(
                        ErrorCode::Invalid,
                        format!("attachments may total at most {} MB", MAX_ATTACHMENTS >> 20),
                    ));
                }
                Ok(())
            })
            .await?;
        self.dispatcher
            .answer(AttachToDraft {
                draft,
                id: None,
                blob,
                name,
                mime,
                size: size.try_into().expect("sizes fit in i64"),
            })
            .await
    }

    pub async fn detach_from_draft(
        &self,
        draft: DraftId,
        attachment: DraftAttachmentId,
    ) -> Response {
        self.dispatcher
            .answer(DetachFromDraft { draft, attachment })
            .await
    }

    pub async fn delete_draft(&self, draft: DraftId) -> Response {
        self.dispatcher.answer(DeleteDraft { id: draft }).await
    }

    /// Refuses drafts that could not be sent. A draft edited meanwhile in
    /// another tab may still turn out unsendable; it then fails in the
    /// outbox.
    pub async fn send_draft(&self, draft: DraftId) -> Response {
        let id = draft.clone();
        self.reader
            .read(move |conn| {
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
        self.dispatcher
            .answer(SendDraft {
                id: draft,
                delay_ms: self.send_delay.as_millis().try_into().expect("delay fits"),
            })
            .await
    }

    pub async fn cancel_send(&self, item: OutboxId) -> Response {
        self.dispatcher.answer(CancelSend { item }).await
    }

    pub async fn keep_sent(&self, item: OutboxId) -> Response {
        self.dispatcher.answer(KeepSent { item }).await
    }

    /// Takes the refused changes of `op` back to what the provider has.
    pub async fn keep_observed(&self, account: String, op: Option<TxLogId>) -> Response {
        let changes = self
            .reader
            .read(move |conn| {
                let tx = conn.transaction().map_err(|e| internal(e.into()))?;
                outbox::keeping_observed(&tx, &account, op).map_err(internal)
            })
            .await?;
        if changes.is_empty() {
            return Err(error(
                ErrorCode::Conflict,
                "nothing refused is left to keep".into(),
            ));
        }
        self.dispatcher.answer(SetLabels { changes }).await
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

/// `type/subtype`, optionally with parameters.
fn is_mime_type(mime: &str) -> bool {
    let essence = mime.split(';').next().expect("split yields one");
    let token = |t: &str| {
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    !mime.contains(char::is_control)
        && essence
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
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
