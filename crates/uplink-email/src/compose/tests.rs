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
    assert_eq!(queued.state, OutboxState::Queued);
    assert_eq!(queued.draft.content.subject, "Lunch");

    let mut live = db.hub.subscribe();
    let cancel = db.dispatcher.undo(sent).await.unwrap();
    assert_eq!(
        live.recv().await.unwrap().events,
        [
            ServerEvent::DraftsChanged(DraftsChanged {
                drafts: vec![DraftId("d".into())]
            }),
            ServerEvent::OutboxChanged(OutboxChanged { items: vec![item] }),
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
    let delay: Vec<i64> = db.query("SELECT delay_ms FROM mail_outbox");
    assert_eq!(delay, [10_000]);
}

#[tokio::test]
async fn sending_cannot_be_undone_once_started() {
    let db = Db::new();
    let (sent, item) = queue(&db).await;
    db.dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap();
    let undo = db.dispatcher.undo(sent).await;
    assert!(
        matches!(&undo, Err(UndoError::Conflict { conflict, .. }) if conflict.0 == "the message is being sent"),
        "{undo:?}"
    );
    // Claimed once only.
    let again = db
        .dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap_err();
    assert!(again.is::<Conflict>());

    // A transient failure requeues it, cancellable again; a permanent one
    // fails it, and cancelling returns the draft for fixing.
    db.dispatcher
        .observe("a".into(), SendFailed { item, error: None })
        .await
        .unwrap();
    assert_eq!(db.outbox()[0].state, OutboxState::Queued);
    db.dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap();
    db.dispatcher
        .observe(
            "a".into(),
            SendFailed {
                item,
                error: Some("Invalid To header".into()),
            },
        )
        .await
        .unwrap();
    let [failed] = &db.outbox()[..] else { panic!() };
    assert_eq!(failed.state, OutboxState::Failed);
    assert_eq!(failed.error.as_deref(), Some("Invalid To header"));
    db.dispatcher.undo(sent).await.unwrap();
    assert!(db.outbox().is_empty());
    assert_eq!(db.drafts().len(), 1);
}

#[tokio::test]
async fn sent_mail_joins_its_provider_copy() {
    let db = Db::new();
    let store = db.store();
    let (sent, item) = queue(&db).await;
    let message_id: Vec<String> = db.query("SELECT message_id FROM mail_outbox");
    db.dispatcher
        .observe("a".into(), SendStarted { item })
        .await
        .unwrap();
    let raw_sent = build_message(
        "a@x",
        &content("Bob <b@x>", "Lunch", "<p>noon?</p>"),
        vec![],
        &message_id[0],
        &ReplyContext::default(),
        1_800_000_000,
    )
    .unwrap();
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
    assert_eq!(db.outbox()[0].state, OutboxState::Sent);
    let messages: Vec<String> = db.query("SELECT message_id FROM message");
    assert_eq!(messages, message_id);
    assert!(matches!(
        db.dispatcher.undo(sent).await,
        Err(UndoError::Conflict { .. })
    ));

    // The provider adds headers: a second variant of the same message.
    let mut copy = b"X-Provider: 1\r\n".to_vec();
    copy.extend(&raw_sent);
    let mut live = db.hub.subscribe();
    db.observe(
        "a",
        observed(
            labels(&["SENT"]),
            vec![new_entry(&store, "g1", &["SENT"], &copy)],
            vec![],
            &[],
        ),
    )
    .await;
    let events = live.recv().await.unwrap().events.clone();
    assert!(
        events.contains(&ServerEvent::OutboxChanged(OutboxChanged {
            items: vec![item]
        })),
        "{events:?}"
    );
    assert!(db.outbox().is_empty());
    let variants: Vec<i64> = db.query("SELECT count(*) FROM message_variant GROUP BY message");
    assert_eq!(variants, [2]);
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
