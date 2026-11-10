use super::*;
use crate::{
    parse::parse,
    tests::{labels, new_entry, observed, raw, service::mail_with, Db},
};
use uplink_protocol::{ErrorCode, ServerResponse, TxLogId};
use uplink_tx_log::UndoError;

fn splice(field: DraftField, offset: u32, delete: &str, insert: &str) -> Splice {
    Splice {
        field,
        offset,
        delete: delete.into(),
        insert: insert.into(),
    }
}

fn content(to: &str, subject: &str, body: &str) -> DraftContent {
    DraftContent {
        to: to.into(),
        cc: String::new(),
        bcc: String::new(),
        subject: subject.into(),
        body: body.into(),
    }
}

fn put(id: &str, content: DraftContent) -> PutDraft {
    PutDraft {
        id: DraftId(id.into()),
        account: "a".into(),
        revision: 0,
        content,
        attachments: vec![],
        in_reply_to: None,
    }
}

fn attach(id: &str, name: &str) -> AttachToDraft {
    AttachToDraft {
        draft: DraftId(id.into()),
        id: None,
        blob: BlobId(VariantHash::of(name.as_bytes()).0),
        name: name.into(),
        mime: "text/plain".into(),
        size: name.len() as i64,
    }
}

fn edit(id: &str, revision: i64, splice: Splice) -> EditDraft {
    EditDraft {
        id: DraftId(id.into()),
        revision: Some(revision),
        splice,
    }
}

impl Db {
    fn drafts(&self) -> Vec<Draft> {
        let mut conn = self.reader();
        let tx = conn.transaction().unwrap();
        drafts(&tx).unwrap()
    }

    fn outbox(&self) -> Vec<OutboxItem> {
        let mut conn = self.reader();
        let tx = conn.transaction().unwrap();
        outbox(&tx).unwrap()
    }

    async fn dispatch(&self, op: impl Op) -> TxLogId {
        self.dispatcher.dispatch(op).await.unwrap()
    }

    async fn conflict(&self, op: impl Op) -> String {
        let e = self.dispatcher.dispatch(op).await.unwrap_err();
        e.downcast::<Conflict>().unwrap().0
    }
}

#[test]
fn splices_count_utf16_units() {
    let text = "a😀b";
    assert_eq!(byte_offset(text, 0), Some(0));
    assert_eq!(byte_offset(text, 1), Some(1));
    assert_eq!(byte_offset(text, 2), None, "inside the surrogate pair");
    assert_eq!(byte_offset(text, 3), Some(5));
    assert_eq!(byte_offset(text, 4), Some(6));
    assert_eq!(byte_offset(text, 5), None);
    let s = |offset, delete, insert| splice(DraftField::Body, offset, delete, insert);
    assert_eq!(apply_splice(text, &s(3, "b", "c")).as_deref(), Some("a😀c"));
    assert_eq!(apply_splice(text, &s(1, "😀", "")).as_deref(), Some("ab"));
    assert_eq!(apply_splice(text, &s(4, "", "!")).as_deref(), Some("a😀b!"));
    assert_eq!(apply_splice(text, &s(1, "b", "")), None);
}

#[tokio::test]
async fn edits_apply_at_their_revision() {
    let db = Db::new();
    let mut live = db.hub.subscribe();
    db.dispatch(put("d", content("", "Hi", "<p>x</p>"))).await;
    assert_eq!(
        live.recv().await.unwrap().events,
        [ServerEvent::DraftsChanged(DraftsChanged {
            drafts: vec![DraftId("d".into())]
        })]
    );

    db.dispatch(edit("d", 0, splice(DraftField::Subject, 2, "", " there")))
        .await;
    let entry = live.recv().await.unwrap();
    assert_eq!(entry.description, "Edited draft 'Hi there'");
    assert_eq!(
        entry.events,
        [ServerEvent::DraftEdited(DraftEdited {
            draft: DraftId("d".into()),
            revision: 1,
            splice: splice(DraftField::Subject, 2, "", " there"),
        })]
    );
    db.dispatch(edit("d", 1, splice(DraftField::To, 0, "", "b@x")))
        .await;

    // Based on a revision since superseded: refused, even though the text
    // it deletes is still there.
    assert_eq!(
        db.conflict(edit("d", 1, splice(DraftField::Body, 3, "x", "y")))
            .await,
        "the draft is at revision 2, not 1"
    );
    assert_eq!(
        db.conflict(edit("d", 2, splice(DraftField::Body, 3, "z", "y")))
            .await,
        "the draft's text changed"
    );
    assert_eq!(
        db.conflict(edit("e", 0, splice(DraftField::Body, 0, "", "y")))
            .await,
        "the draft no longer exists"
    );

    let [draft] = &db.drafts()[..] else { panic!() };
    assert_eq!(draft.revision, 2);
    assert_eq!(draft.content, content("b@x", "Hi there", "<p>x</p>"));
}

#[tokio::test]
async fn edits_undo_one_at_a_time() {
    let db = Db::new();
    db.dispatch(put("d", content("", "", "ab"))).await;
    let first = db
        .dispatch(edit("d", 0, splice(DraftField::Body, 1, "", "X")))
        .await;
    let second = db
        .dispatch(edit("d", 1, splice(DraftField::Body, 3, "", "Y")))
        .await;
    let body = |db: &Db| db.drafts()[0].content.body.clone();
    assert_eq!(body(&db), "aXbY");

    // Out of order: the inverse finds its text where it expects it.
    db.dispatcher.undo(first).await.unwrap();
    assert_eq!(body(&db), "abY");
    let undo = db.dispatcher.undo(second).await;
    assert!(
        matches!(undo, Err(UndoError::Conflict { .. })),
        "Y moved: {undo:?}"
    );
    // Undoing the undo is a redo.
    db.dispatcher.undo(TxLogId(second.0 + 1)).await.unwrap();
    assert_eq!(body(&db), "aXbY");
    assert_eq!(db.drafts()[0].revision, 4);
}

#[tokio::test]
async fn discarded_drafts_are_restored_by_undo() {
    let db = Db::new();
    db.dispatch(put("d", content("b@x", "S", "body"))).await;
    db.dispatch(edit("d", 0, splice(DraftField::Body, 4, "", "!")))
        .await;
    let delete = db
        .dispatch(DeleteDraft {
            id: DraftId("d".into()),
        })
        .await;
    assert!(db.drafts().is_empty());
    db.dispatcher.undo(delete).await.unwrap();
    let [draft] = &db.drafts()[..] else { panic!() };
    assert_eq!(draft.revision, 1);
    assert_eq!(draft.content.body, "body!");
    assert_eq!(
        db.conflict(put("d", content("", "", ""))).await,
        "draft d exists already"
    );
}

async fn queue(db: &Db) -> (TxLogId, OutboxId) {
    db.dispatch(put("d", content("Bob <b@x>", "Lunch", "<p>noon?</p>")))
        .await;
    let sent = db
        .dispatch(SendDraft {
            id: DraftId("d".into()),
            delay_ms: 10_000,
        })
        .await;
    let [item] = &db.outbox()[..] else { panic!() };
    (sent, item.id)
}

#[tokio::test]
async fn sending_is_undone_while_queued() {
    let db = Db::new();
    let (sent, item) = queue(&db).await;
    assert!(db.drafts().is_empty());
    let [queued] = &db.outbox()[..] else { panic!() };
    assert_eq!(queued.state, SendState::Queued);
    assert!(queued.wanted);
    assert_eq!(queued.draft.content.subject, "Lunch");

    let mut live = db.hub.subscribe();
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(
        live.recv().await.unwrap().events,
        [
            ServerEvent::DraftsChanged(DraftsChanged {
                drafts: vec![DraftId("d".into())]
            }),
            ServerEvent::OutboxChanged(uplink_protocol::compose::OutboxChanged {
                accounts: vec!["a".into()]
            }),
        ]
    );
    assert!(db.outbox().is_empty());
    assert_eq!(db.drafts()[0].content.subject, "Lunch");

    // Redo queues it afresh, with the same delay.
    db.dispatcher.undo(cancel).await.unwrap();
    let [requeued] = &db.outbox()[..] else {
        panic!()
    };
    assert_ne!(requeued.id, item);
    let delay: Vec<i64> = db.query("SELECT delay_ms FROM mail_send");
    assert_eq!(delay, [10_000]);
}

async fn start(db: &Db, item: OutboxId) {
    db.dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap();
}

async fn fail(db: &Db, item: OutboxId, outcome: SendOutcome) {
    db.dispatcher
        .observe("a".into(), SendFailed { item, outcome })
        .await
        .unwrap();
}

/// What the outbox shows of the send: its state and whether it is wanted.
fn shown(db: &Db) -> Vec<(SendState, bool)> {
    db.outbox().iter().map(|i| (i.state, i.wanted)).collect()
}

/// Once handed to the provider, a send cancelled is taken back if it did
/// not go out; a refused one waits for the user, who takes it back at
/// once; one whose outcome is unknown is sent again once known not sent.
#[tokio::test]
async fn sends_handed_over_are_settled_by_their_outcome() {
    let db = Db::new();
    let (sent, item) = queue(&db).await;
    start(&db, item).await;
    // Claimed once only.
    let again = db
        .dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap_err();
    assert!(again.is::<Conflict>());

    // Cancelled while being sent: no longer wanted, until the outcome.
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(shown(&db), [(SendState::Sending, false)]);
    assert_eq!(
        db.query::<String>(&format!(
            "SELECT description FROM tx_log WHERE id = {}",
            cancel.0
        )),
        ["Cancelled sending 'Lunch', which may have gone out"]
    );
    // Not sent after all: taken back.
    fail(&db, item, SendOutcome::NotSent).await;
    assert!(db.outbox().is_empty());
    assert_eq!(db.drafts().len(), 1);

    // Refused: it waits; cancelling takes it back at once.
    let (sent, item) = {
        let sent = db
            .dispatch(SendDraft {
                id: DraftId("d".into()),
                delay_ms: 10_000,
            })
            .await;
        (sent, db.outbox()[0].id)
    };
    start(&db, item).await;
    fail(&db, item, SendOutcome::Refused("Invalid To header".into())).await;
    let [refused] = &db.outbox()[..] else {
        panic!()
    };
    assert_eq!((refused.state, refused.wanted), (SendState::Refused, true));
    assert_eq!(refused.error.as_deref(), Some("Invalid To header"));
    db.dispatcher.undo(sent).await.unwrap();
    assert!(db.outbox().is_empty());
    assert_eq!(db.drafts().len(), 1);

    // Unknown: once known not sent, it is sent again.
    let sent = db
        .dispatch(SendDraft {
            id: DraftId("d".into()),
            delay_ms: 10_000,
        })
        .await;
    let item = db.outbox()[0].id;
    start(&db, item).await;
    fail(&db, item, SendOutcome::Unknown("timed out".into())).await;
    let [unknown] = &db.outbox()[..] else {
        panic!()
    };
    assert_eq!((unknown.state, unknown.wanted), (SendState::Unknown, true));
    assert_eq!(unknown.error.as_deref(), Some("timed out"));
    db.dispatcher
        .observe("a".into(), SendNotFound { item })
        .await
        .unwrap();
    assert_eq!(shown(&db), [(SendState::Queued, true)]);

    // And cancelled while unknown, known not sent, it is taken back.
    start(&db, item).await;
    fail(&db, item, SendOutcome::Unknown("timed out".into())).await;
    db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(shown(&db), [(SendState::Unknown, false)]);
    db.dispatcher
        .observe("a".into(), SendNotFound { item })
        .await
        .unwrap();
    assert!(db.outbox().is_empty());
    assert_eq!(db.drafts().len(), 1);
}

/// The provider's copy, as synced: the message as sent, with a header
/// added, in Sent.
async fn sync_copy(db: &Db, raw_sent: &[u8]) {
    let mut copy = b"X-Provider: 1\r\n".to_vec();
    copy.extend(raw_sent);
    db.observe(
        "a",
        observed(
            labels(&["SENT"]),
            vec![new_entry(&db.store(), "g1", &["SENT"], &copy)],
            vec![],
            &[],
        ),
    )
    .await;
}

fn built(message_id: &str) -> Vec<u8> {
    build_message(
        "a@x",
        &content("Bob <b@x>", "Lunch", "<p>noon?</p>"),
        vec![],
        message_id,
        &ReplyContext::default(),
        1_800_000_000,
    )
    .unwrap()
}

#[tokio::test]
async fn sent_mail_joins_its_provider_copy() {
    let db = Db::new();
    let store = db.store();
    let (sent, item) = queue(&db).await;
    let message_id: Vec<String> = db.query("SELECT message_id FROM mail_send");
    start(&db, item).await;
    let raw_sent = built(&message_id[0]);
    db.dispatcher
        .observe(
            "a".into(),
            SendSucceeded {
                item,
                variant: store.put(&raw_sent).unwrap(),
                size_bytes: raw_sent.len() as i64,
                parsed: parse(&raw_sent),
            },
        )
        .await
        .unwrap();
    assert_eq!(shown(&db), [(SendState::Sent, true)]);
    let messages: Vec<String> = db.query("SELECT message_id FROM message");
    assert_eq!(messages, message_id);

    // The provider adds headers: a second variant of the same message.
    let mut live = db.hub.subscribe();
    sync_copy(&db, &raw_sent).await;
    let events = live.recv().await.unwrap().events.clone();
    assert!(
        events.contains(&ServerEvent::OutboxChanged(
            uplink_protocol::compose::OutboxChanged {
                accounts: vec!["a".into()]
            }
        )),
        "{events:?}"
    );
    assert!(db.outbox().is_empty());
    let variants: Vec<i64> = db.query("SELECT count(*) FROM message_variant GROUP BY message");
    assert_eq!(variants, [2]);

    // Undoing the send now can't take it back: it needs a decision, which
    // keeping it sent (undoing the undo) makes.
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(shown(&db), [(SendState::Sent, false)]);
    db.dispatcher.undo(cancel).await.unwrap();
    assert!(db.outbox().is_empty());
}

/// A send whose outcome is unknown is sent if its copy turns up in Sent.
#[tokio::test]
async fn an_unknown_send_is_settled_by_its_copy() {
    let db = Db::new();
    let (_, item) = queue(&db).await;
    let message_id: Vec<String> = db.query("SELECT message_id FROM mail_send");
    start(&db, item).await;
    fail(&db, item, SendOutcome::Unknown("timed out".into())).await;
    sync_copy(&db, &built(&message_id[0])).await;
    assert!(db.outbox().is_empty());
    let settled: Vec<String> = db.query(
        "SELECT state || ' ' || synced || ' ' || (message = (SELECT id FROM message))
         FROM mail_send",
    );
    assert_eq!(settled, ["sent 1 1"]);
}

#[test]
fn addresses_as_typed() {
    assert_eq!(
        parse_addresses("Bob <b@x.org>, c@y.org,\n \"Doe, Jane\" <j@z.org>").unwrap(),
        [
            (Some("Bob".into()), "b@x.org".into()),
            (None, "c@y.org".into()),
            (Some("Doe, Jane".into()), "j@z.org".into()),
        ]
    );
    assert!(parse_addresses("  ").unwrap().is_empty());
    assert!(parse_addresses("bob").is_err());
    assert!(parse_addresses("b@x.org, @y").is_err());
}

#[test]
fn messages_are_built_sanitized_with_reply_headers() {
    let original = raw(
        "orig@x",
        "b@x",
        "Tue, 3 Nov 2026 10:00:00 +0000",
        "Lunch",
        "?",
    );
    let mut original_with_refs = b"References: <first@x>\r\n".to_vec();
    original_with_refs.extend(&original);
    let reply = ReplyContext::of("orig@x", &original_with_refs);
    let raw = build_message(
        "a@x",
        &DraftContent {
            to: "Bob <b@x>".into(),
            cc: "c@x".into(),
            bcc: "d@x".into(),
            subject: "Re: Lunch".into(),
            body: "<p>Noon <b>works</b><script>alert(1)</script></p>".into(),
        },
        vec![],
        "new@a",
        &reply,
        1_800_000_000,
    )
    .unwrap();
    let text = String::from_utf8(raw.clone()).unwrap();
    assert!(!text.contains("script"), "{text}");
    let message = MessageParser::new().parse(&raw).unwrap();
    assert_eq!(message.message_id(), Some("new@a"));
    assert_eq!(message.in_reply_to().as_text(), Some("orig@x"));
    assert_eq!(
        message.references().as_text_list().unwrap(),
        ["first@x", "orig@x"]
    );
    assert_eq!(message.subject(), Some("Re: Lunch"));
    assert_eq!(message.date().unwrap().to_timestamp(), 1_800_000_000);
    assert_eq!(
        message.bcc().unwrap().first().unwrap().address(),
        Some("d@x")
    );
    assert_eq!(message.body_text(0).unwrap().trim(), "Noon works");
    assert!(message.body_html(0).unwrap().contains("<b>works</b>"));

    let none = build_message(
        "a@x",
        &content("", "", ""),
        vec![],
        "n@a",
        &ReplyContext::default(),
        0,
    );
    assert_eq!(
        none.unwrap_err().to_string(),
        "the message has no recipients"
    );
}

#[tokio::test]
async fn unsendable_drafts_are_refused() {
    let db = Db::new();
    let mail = mail_with(&db).await;
    let id = DraftId("d".into());
    mail.create_draft(id.clone(), "a".into(), content("bob", "", ""), None)
        .await
        .unwrap();
    let e = mail.send_draft(id.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    assert_eq!(e.message, "To: not an address: bob");
    let e = mail
        .create_draft(DraftId("e".into()), "z".into(), content("", "", ""), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let ServerResponse::Drafts { drafts, .. } = mail.list_drafts().await.unwrap() else {
        panic!()
    };
    assert_eq!(drafts.len(), 1);
}

#[tokio::test]
async fn files_are_attached_and_detached_by_undoable_ops() {
    let db = Db::new();
    db.dispatch(put("d", content("b@x", "S", "body"))).await;
    let mut live = db.hub.subscribe();
    let first = db.dispatch(attach("d", "a.txt")).await;
    let entry = live.recv().await.unwrap();
    assert_eq!(entry.description, "Attached 'a.txt' to draft 'S'");
    assert_eq!(
        entry.events,
        [ServerEvent::DraftsChanged(DraftsChanged {
            drafts: vec![DraftId("d".into())]
        })]
    );
    db.dispatch(attach("d", "b.txt")).await;
    let names = |db: &Db| -> Vec<String> {
        db.drafts()
            .into_iter()
            .flat_map(|d| d.attachments)
            .map(|a| a.name)
            .collect()
    };
    assert_eq!(names(&db), ["a.txt", "b.txt"]);
    // Attaching leaves the text's revision as it is.
    assert_eq!(db.drafts()[0].revision, 0);

    // Undoing the attach detaches; undoing that reattaches, as before.
    let a = db.drafts()[0].attachments[0].clone();
    db.dispatcher.undo(first).await.unwrap();
    assert_eq!(names(&db), ["b.txt"]);
    db.dispatcher.undo(TxLogId(first.0 + 2)).await.unwrap();
    assert_eq!(db.drafts()[0].attachments[0], a);
    assert_eq!(
        db.conflict(DetachFromDraft {
            draft: DraftId("d".into()),
            attachment: DraftAttachmentId(99),
        })
        .await,
        "the file is no longer attached"
    );
    assert_eq!(
        db.conflict(attach("e", "c.txt")).await,
        "the draft no longer exists"
    );

    // Discarding and sending keep them with the draft.
    let delete = db
        .dispatch(DeleteDraft {
            id: DraftId("d".into()),
        })
        .await;
    let left: Vec<i64> = db.query("SELECT count(*) FROM draft_attachment");
    assert_eq!(left, [0]);
    db.dispatcher.undo(delete).await.unwrap();
    assert_eq!(names(&db), ["a.txt", "b.txt"]);
    let sent = db
        .dispatch(SendDraft {
            id: DraftId("d".into()),
            delay_ms: 10_000,
        })
        .await;
    assert_eq!(db.outbox()[0].draft.attachments.len(), 2);
    db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(names(&db), ["a.txt", "b.txt"]);
}

#[test]
fn messages_are_built_with_attached_files() {
    let pdf = b"%PDF-1.4 \x00\xff binary".to_vec();
    let raw = build_message(
        "a@x",
        &content("b@x", "Report", "<p>Attached.</p>"),
        vec![
            File {
                name: "Résumé.pdf".into(),
                mime: "application/pdf".into(),
                content: pdf.clone(),
            },
            File {
                name: "notes.txt".into(),
                mime: "text/plain".into(),
                content: b"hello".to_vec(),
            },
        ],
        "n@a",
        &ReplyContext::default(),
        0,
    )
    .unwrap();
    let listed = crate::parse::attachments(&raw);
    assert_eq!(
        listed
            .iter()
            .map(|a| (a.name.as_deref(), a.mime.as_str(), a.size, a.inline))
            .collect::<Vec<_>>(),
        [
            (
                Some("Résumé.pdf"),
                "application/pdf",
                pdf.len() as u64,
                false
            ),
            (Some("notes.txt"), "text/plain", 5, false),
        ]
    );
    assert_eq!(crate::parse::attachment(&raw, 0).unwrap().2, pdf);
    let message = MessageParser::new().parse(&raw).unwrap();
    assert_eq!(message.body_text(0).unwrap().trim(), "Attached.");
}

#[tokio::test]
async fn uploaded_files_are_attached_within_limits() {
    let db = Db::new();
    let mail = mail_with(&db).await;
    let id = DraftId("d".into());
    mail.create_draft(id.clone(), "a".into(), content("b@x", "", ""), None)
        .await
        .unwrap();
    let uploaded = mail.put_blob(b"hello".to_vec()).await.unwrap();
    assert_eq!(uploaded.size, 5);
    let attach = |name: &str, mime: &str, blob: &str| {
        mail.attach_to_draft(id.clone(), BlobId(blob.into()), name.into(), mime.into())
    };
    let blob = uploaded.blob.0.as_str();
    attach("hello.txt", "text/plain; charset=utf-8", blob)
        .await
        .unwrap();
    let ServerResponse::Drafts { drafts, .. } = mail.list_drafts().await.unwrap() else {
        panic!()
    };
    let [a] = &drafts[0].attachments[..] else {
        panic!()
    };
    assert_eq!((a.name.as_str(), a.size), ("hello.txt", 5));

    let invalid = [
        attach("", "text/plain", blob).await,
        attach("a\nb", "text/plain", blob).await,
        attach("a", "text", blob).await,
        attach("a", "text/", blob).await,
        attach("a", "text/plain\r\nX: y", blob).await,
        attach("a", "text/plain", "../../etc/passwd").await,
    ];
    for e in invalid {
        assert_eq!(e.unwrap_err().code, ErrorCode::Invalid);
    }
    let missing = VariantHash::of(b"never uploaded").0;
    assert_eq!(
        attach("a", "text/plain", &missing).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    let e = mail
        .attach_to_draft(
            DraftId("e".into()),
            uploaded.blob.clone(),
            "a".into(),
            "text/plain".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);

    let big = mail
        .put_blob(vec![0; (crate::service::MAX_ATTACHMENTS - 4) as usize])
        .await
        .unwrap();
    let e = attach("big.bin", "application/octet-stream", &big.blob.0)
        .await
        .unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Invalid, "attachments may total at most 25 MB")
    );
}
