//! POP3 sync, and SMTP sending, against Stalwart (see `common::stalwart`):
//! its POP3 upgraded by STLS. Mail reaches the maildrop as other clients
//! put it there, over IMAP, or as Bob sends it, by SMTP.

mod common;

use common::{
    eventually, message,
    stalwart::{append, find, Stalwart, ALICE, BOB},
    uplink::{label_set, Uplink},
};
use std::time::Duration;
use uplink_email::pop3::sync::Pop3Sync;
use uplink_protocol::mail::MailProvider;

/// uplink's side of Alice's account.
fn provider(stalwart: &Stalwart, delete_after_download: bool) -> Pop3Sync {
    let account = stalwart.pop3_account("alice", delete_after_download);
    let sign_in = stalwart.sign_in(account.login());
    Pop3Sync::new(account, sign_in, stalwart.tls.clone())
}

fn entries(uplink: &Uplink) -> i64 {
    uplink
        .reader()
        .query_row("SELECT count(*) FROM mail_entry", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_mail_and_keeps_its_labels_in_uplink() {
    let uplink = Uplink::new(MailProvider::Pop3);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("first", "hello world")).await;
    let sync = uplink.start(provider(&stalwart, false), ALICE);
    let first = uplink.arrived("first").await;
    assert_eq!(label_set(&uplink.labels(first)), ["INBOX", "UNREAD"]);
    let (_, _, thread) = uplink.message_of(first);
    assert!(thread.unwrap().starts_with("first."));

    // Mail arriving later; what was downloaded is not again.
    append(&mut other, "INBOX", &message("second", "")).await;
    let second = uplink.arrived("second").await;
    assert_eq!(entries(&uplink), 2);

    // Labels change in uplink alone, at once; the mail stays on the
    // server, as it was.
    uplink
        .mail
        .modify_labels(
            vec![first],
            vec![uplink.label("STARRED")],
            vec![uplink.label("INBOX"), uplink.label("UNREAD")],
        )
        .await
        .unwrap();
    eventually("the change to take effect", || async {
        (uplink.labels(first) == [("STARRED".into(), true, true)]).then_some(())
    })
    .await;
    for tag in ["first", "second"] {
        let (_, flags) = find(&mut other, "INBOX", tag).await.unwrap();
        assert!(!flags.contains(&"Flagged".into()), "{flags:?}");
    }
    assert_eq!(label_set(&uplink.labels(second)), ["INBOX", "UNREAD"]);

    // A message another client deletes stays downloaded.
    other.session.select("INBOX").await.unwrap();
    let _: Vec<_> = futures_util::TryStreamExt::try_collect(
        other
            .session
            .store("1:*", "+FLAGS (\\Deleted)")
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let _: Vec<_> = futures_util::TryStreamExt::try_collect(other.session.expunge().await.unwrap())
        .await
        .unwrap();
    append(&mut other, "INBOX", &message("third", "")).await;
    uplink.arrived("third").await;
    assert_eq!(entries(&uplink), 3);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn deletes_mail_from_the_server_once_downloaded_if_told() {
    let uplink = Uplink::new(MailProvider::Pop3);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("first", "")).await;
    append(&mut other, "INBOX", &message("second", "")).await;
    let sync = uplink.start(provider(&stalwart, true), ALICE);
    uplink.arrived("first").await;
    uplink.arrived("second").await;
    eventually("the server to be emptied", || async {
        let mut c = stalwart.client("alice").await;
        let exists = c.session.select("INBOX").await.unwrap().exists;
        (exists == 0).then_some(())
    })
    .await;
    // Downloaded still, once.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(entries(&uplink), 2);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_downloads_the_maildrop_in_steps() {
    let uplink = Uplink::new(MailProvider::Pop3);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    for i in 0..120 {
        append(&mut other, "INBOX", &message(&format!("m{i:03}"), "")).await;
    }
    let sync = uplink.start(provider(&stalwart, false), ALICE);
    eventually("the listing to be done", || async {
        uplink.synced().then_some(())
    })
    .await;
    assert_eq!(entries(&uplink), 120);
    let steps: i64 = uplink
        .reader()
        .query_row(
            "SELECT count(*) FROM tx_log WHERE op_type = 'mail.observed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(steps, 3);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_mail_by_smtp_and_files_it_in_sent() {
    let uplink = Uplink::new(MailProvider::Pop3);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("original", "lunch?")).await;
    let sync = uplink.start(provider(&stalwart, false), ALICE);
    let original = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original);

    // A reply to Bob: delivered, and filed in Sent by uplink as the same
    // message, in the original's conversation.
    let reply = format!("reply {}", common::nonce());
    uplink.send(BOB, &reply, Some(original_message)).await;
    eventually("the reply to reach Bob", || async {
        find(&mut stalwart.client("bob").await, "INBOX", "reply").await
    })
    .await;
    let sent = eventually("the reply's copy to be filed", || async {
        uplink.entry("reply")
    })
    .await;
    assert_eq!(label_set(&uplink.labels(sent)), ["SENT"]);
    let (_, message_id, thread) = uplink.message_of(sent);
    assert!(message_id.ends_with("@test.uplink"), "{message_id}");
    assert_eq!(thread, original_thread);
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;

    // Bob's answer comes by SMTP, as mail does: dated as delivered.
    let bob = stalwart.pop3_account("bob", false);
    let answer = format!(
        "Message-ID: <answer@uplink.test>\r\nFrom: {BOB}\r\nTo: {ALICE}\r\n\
         Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: answer 1\r\n\r\nyes\r\n"
    );
    uplink_email::mail_server::smtp::send(
        bob.login(),
        &stalwart.sign_in(bob.login()),
        &stalwart.tls,
        answer.as_bytes(),
    )
    .await
    .unwrap();
    let answer = uplink.arrived("answer").await;
    let received_at: i64 = uplink
        .reader()
        .query_row(
            "SELECT received_at FROM mail_entry WHERE id = ?1",
            [answer.0],
            |r| r.get(0),
        )
        .unwrap();
    // Not its Date, from before the test.
    assert!(received_at > 1_793_700_000_000, "{received_at}");
    sync.abort();
}
