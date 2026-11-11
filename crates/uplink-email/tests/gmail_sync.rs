//! Gmail sync against a provider (emulated, or real: see `common`).

mod common;

use common::{
    eventually, message,
    uplink::{label_set, Uplink},
    Provider,
};
use std::time::Duration;
use uplink_email::{
    gmail::{api::Gmail, oauth, sync::GmailSync},
    oauth::Credentials,
};
use uplink_protocol::{compose::SendState, mail::MailProvider, ServerResponse};

/// Gmail's sync task, polling quickly.
fn start_sync(uplink: &Uplink, provider: &Provider) -> tokio::task::JoinHandle<()> {
    let http = reqwest::Client::new();
    let credentials = Credentials::new(
        http.clone(),
        oauth::grant(&provider.account),
        provider.key.clone(),
    );
    uplink.start(
        GmailSync::new(Gmail::new(
            http,
            &provider.account.endpoints.api,
            credentials,
        )),
        &provider.account.address,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_mailbox_and_pushes_changes() {
    let uplink = Uplink::new(MailProvider::Gmail);
    let provider = Provider::start(uplink.dir.path(), None).await;

    // Mail present before the first sync, and mail arriving afterwards.
    let first = provider.deliver(&message("first", "hello world")).await;
    let sync = start_sync(&uplink, &provider);
    let first_entry = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first_entry),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let second = provider
        .deliver(&message("second", "a quixotic body"))
        .await;
    let second_entry = uplink.arrived("second").await;

    // Archiving in uplink archives in Gmail; undo brings it back.
    let ServerResponse::Dispatched {
        tx_log_id: archived,
    } = uplink
        .mail
        .modify_labels(vec![first_entry], vec![], vec![uplink.label("INBOX")])
        .await
        .unwrap()
    else {
        panic!()
    };
    eventually("the archive to reach Gmail", || async {
        (provider.labels(&first).await == ["UNREAD"]).then_some(())
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first_entry) == [("UNREAD".into(), true, true)]).then_some(())
    })
    .await;
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach Gmail", || async {
        (provider.labels(&first).await == ["INBOX", "UNREAD"]).then_some(())
    })
    .await;

    // A change made in Gmail shows up in uplink.
    provider.modify(&second, &["STARRED"], &["UNREAD"]).await;
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(second_entry)) == ["INBOX", "STARRED"]).then_some(())
    })
    .await;

    // Search covers subject, body and sender.
    for query in ["second", "quixot", "tester@uplink"] {
        let ServerResponse::Conversations { conversations, .. } = uplink
            .mail
            .list_entries(query.into(), None, 10)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(
            conversations
                .iter()
                .any(|c| c.latest.id == second_entry
                    || c.earlier.iter().any(|m| m.id == second_entry)),
            "{query} does not find the message"
        );
    }

    // A restarted sync resumes from the recorded history id: idle polls
    // record nothing, and new mail still arrives.
    sync.abort();
    let _ = sync.await;
    let settled = uplink.log_len();
    let sync = start_sync(&uplink, &provider);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(uplink.log_len(), settled, "the restarted sync re-synced");
    provider.deliver(&message("third", "after restart")).await;
    uplink.arrived("third").await;

    sync.abort();
    provider.clean_up().await;
}

/// A change Gmail refuses is recorded as refused, and holds up no other:
/// here, a label deleted in Gmail since uplink last listed the labels.
/// (Listing them again, as the next change seen makes uplink do, takes the
/// label away, and the refused change with it.)
#[tokio::test(flavor = "multi_thread")]
async fn refused_changes_hold_up_no_others() {
    let uplink = Uplink::new(MailProvider::Gmail);
    let provider = Provider::start(uplink.dir.path(), None).await;
    let doomed = provider
        .create_label(&format!("Doomed {}", common::nonce()))
        .await;
    let first = provider.deliver(&message("first", "one")).await;
    let second = provider.deliver(&message("second", "two")).await;
    let sync = start_sync(&uplink, &provider);
    let first_entry = uplink.arrived("first").await;
    let second_entry = uplink.arrived("second").await;
    eventually("the listing to end", || async {
        uplink.synced().then_some(())
    })
    .await;
    provider.delete_label(&doomed).await;

    let label = uplink.label(&doomed);
    let ServerResponse::Dispatched {
        tx_log_id: labelled,
    } = uplink
        .mail
        .modify_labels(vec![first_entry], vec![label], vec![])
        .await
        .unwrap()
    else {
        panic!()
    };
    // Pending; refused or not yet, as the sync may already have tried it.
    assert_eq!(uplink.pending(first_entry, label).0, Some(labelled.0));
    uplink
        .mail
        .modify_labels(vec![second_entry], vec![], vec![uplink.label("INBOX")])
        .await
        .unwrap();
    eventually("the archive to reach Gmail", || async {
        (provider.labels(&second).await == ["UNREAD"]).then_some(())
    })
    .await;
    let refusal = eventually("the label to be refused", || async {
        uplink.logged("mail.refused")
    })
    .await;
    assert!(refusal.contains(&doomed), "{refusal}");
    assert_eq!(provider.labels(&first).await, ["INBOX", "UNREAD"]);

    sync.abort();
    provider.clean_up().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resyncs_fully_once_history_expired() {
    let uplink = Uplink::new(MailProvider::Gmail);
    if common::real() {
        eprintln!("skipped: needs an emulated account, whose history can expire");
        return;
    }
    let provider = Provider::start(uplink.dir.path(), Some(3)).await;
    let kept = provider.deliver(&message("kept", "")).await;
    let deleted = provider.deliver(&message("deleted", "")).await;
    let sync = start_sync(&uplink, &provider);
    let kept_entry = uplink.arrived("kept").await;
    uplink.arrived("deleted").await;
    sync.abort();
    let _ = sync.await;

    // More changes than the history keeps, while uplink is away.
    for _ in 0..2 {
        provider.modify(&kept, &["STARRED"], &[]).await;
        provider.modify(&kept, &[], &["STARRED"]).await;
    }
    provider.modify(&kept, &["STARRED"], &["INBOX"]).await;
    provider.delete(&deleted).await;
    provider.deliver(&message("new", "")).await;
    let recorded: String = uplink
        .reader()
        .query_row(
            "SELECT json_extract(sync_state, '$.history_id') FROM account",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(provider.history_status(&recorded).await, 404);

    let sync = start_sync(&uplink, &provider);
    uplink.arrived("new").await;
    eventually("the deletion to be observed", || async {
        uplink.entry("deleted").is_none().then_some(())
    })
    .await;
    assert_eq!(
        uplink.labels(kept_entry),
        [
            ("STARRED".into(), true, true),
            ("UNREAD".into(), true, true)
        ]
    );
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_an_interrupted_listing() {
    let uplink = Uplink::new(MailProvider::Gmail);
    let provider = Provider::start(uplink.dir.path(), None).await;
    let first = provider.deliver(&message("first", "")).await;
    provider.deliver(&message("second", "")).await;
    let sync = start_sync(&uplink, &provider);
    let first_entry = uplink.arrived("first").await;
    uplink.arrived("second").await;
    sync.abort();
    let _ = sync.await;

    // As if the first listing had been interrupted once it had fetched
    // both: it began at the history id now recorded, with nothing before.
    let db = uplink_storage::open(uplink.dir.path()).unwrap();
    db.execute(
        "UPDATE account SET synced = 0, sync_state = json_object(
           'listing_from', json_extract(sync_state, '$.history_id'), 'before', 0)",
        [],
    )
    .unwrap();
    let synced = || -> bool {
        db.query_row("SELECT synced FROM account", [], |r| r.get(0))
            .unwrap()
    };
    assert!(!synced());
    // Changes while it was away, which the listing leaves to the polls.
    provider.modify(&first, &["STARRED"], &[]).await;
    provider.deliver(&message("third", "")).await;

    let sync = start_sync(&uplink, &provider);
    uplink.arrived("third").await;
    eventually("the star to be observed", || async {
        label_set(&uplink.labels(first_entry))
            .contains(&"STARRED")
            .then_some(())
    })
    .await;
    assert!(synced());
    let state: String = db
        .query_row("SELECT sync_state FROM account", [], |r| r.get(0))
        .unwrap();
    assert!(state.starts_with(r#"{"history_id":"#), "{state}");
    sync.abort();
    provider.clean_up().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_mail_after_the_delay_unless_undone() {
    let uplink = Uplink::new(MailProvider::Gmail);
    let provider = Provider::start(uplink.dir.path(), None).await;
    let me = provider.account.address.clone();
    provider.deliver(&message("original", "lunch?")).await;
    let sync = start_sync(&uplink, &provider);
    let original_entry = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original_entry);

    // A reply, to ourselves so that it arrives: queued for the delay, then
    // sent in the original's thread, and synced back as the same message.
    let nonce = common::nonce();
    let reply = format!("reply {nonce}");
    uplink.send(&me, &reply, Some(original_message)).await;
    assert!(matches!(
        &uplink.outbox().await[..],
        [(_, SendState::Queued)]
    ));
    let reply_entry = uplink.arrived("reply").await;
    let (reply_message, reply_message_id, reply_thread) = uplink.message_of(reply_entry);
    provider.clean_up_later(uplink.external_id(reply_entry));
    assert_eq!(reply_thread, original_thread);
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    // The send and its copy are the same message, under the Message-ID
    // Gmail gave it: the message as sent is a variant of it.
    let (sent_message, our_message_id, copy, variants): (i64, String, String, i64) = uplink
        .reader()
        .query_row(
            "SELECT message, message_id, provider_copy,
                    (SELECT count(*) FROM message_variant v WHERE v.message = s.message)
             FROM mail_send s",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(sent_message, reply_message.0);
    assert_eq!(copy, uplink.external_id(reply_entry));
    assert_ne!(
        reply_message_id, our_message_id,
        "Gmail gives sent mail its own"
    );
    assert_eq!(variants, 2);
    let labels = uplink.labels(reply_entry);
    assert!(label_set(&labels).contains(&"SENT"), "{labels:?}");

    // Undone within the delay: back to drafts, and never sent.
    let undone = format!("undone {nonce}");
    let sent = uplink.send(&me, &undone, None).await;
    uplink
        .dispatcher
        .undo(uplink_protocol::TxLogId(sent))
        .await
        .unwrap();
    let ServerResponse::Drafts { drafts, .. } = uplink.mail.list_drafts().await.unwrap() else {
        panic!()
    };
    assert_eq!(drafts[0].content.subject, undone);
    tokio::time::sleep(common::uplink::SEND_DELAY * 3).await;
    assert!(uplink.outbox().await.is_empty());
    assert!(uplink.entry("undone").is_none());

    sync.abort();
    provider.clean_up().await;
}

/// A send whose draft Gmail already sent, as an attempt whose answer was
/// lost did, is not sent again: its copy is found in the draft's thread.
#[tokio::test(flavor = "multi_thread")]
async fn a_draft_sent_already_is_not_sent_again() {
    let uplink = Uplink::new(MailProvider::Gmail);
    let provider = Provider::start(uplink.dir.path(), None).await;
    let me = provider.account.address.clone();
    let sync = start_sync(&uplink, &provider);
    uplink.settled().await;

    // Queued, then made to look drafted and sent by an earlier attempt,
    // before it is due.
    let (draft, thread, copy) = provider.send_as_draft(&message("lost", "sent once")).await;
    uplink.send(&me, "lost", None).await;
    uplink_storage::rusqlite::Connection::open(uplink_storage::db_path(uplink.dir.path()))
        .unwrap()
        .execute(
            "UPDATE mail_send SET provider_draft = ?1, provider_thread = ?2",
            [&draft, &thread],
        )
        .unwrap();

    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    let told: String = uplink
        .reader()
        .query_row("SELECT provider_copy FROM mail_send", [], |r| r.get(0))
        .unwrap();
    assert_eq!(told, copy);
    assert_eq!(provider.sent_in_thread(&thread).await, [copy]);

    sync.abort();
    provider.clean_up().await;
}
