//! IMAP and SMTP sync against Stalwart (`UPLINK_STALWART`, set by the dev
//! shell and `nix flake check`), a real mail server, one per test. The
//! other side is played as another mail client would, over IMAP.

mod common;

use common::{
    dovecot::Dovecot,
    eventually, message,
    stalwart::{append, find, store, Directory, Stalwart, ALICE, BOB},
    uplink::{label_set, Uplink},
};
use futures_util::TryStreamExt;
use std::time::Duration;
use uplink_config::{ImapAccountConfig, OAuthConfig};
use uplink_email::{
    imap::{session, sync::ImapSync},
    mail_server::{
        sign_in::{self, SignIn},
        smtp,
    },
};
use uplink_protocol::{mail::MailProvider, ServerResponse};

/// uplink's side of Alice's account.
fn provider(stalwart: &Stalwart) -> ImapSync {
    let account = stalwart.account("alice");
    let sign_in = stalwart.sign_in(account.login());
    ImapSync::new(account, sign_in, stalwart.tls.clone())
}

fn dispatched(response: ServerResponse) -> uplink_protocol::TxLogId {
    let ServerResponse::Dispatched { tx_log_id } = response else {
        panic!("{response:?}")
    };
    tx_log_id
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_folders_and_pushes_changes() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    other.session.create("Projects").await.unwrap();

    // Mail present before the first sync, and mail arriving afterwards.
    append(&mut other, "INBOX", &message("first", "hello world")).await;
    let sync = uplink.start(provider(&stalwart), ALICE);
    let first = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let folders: Vec<(String, String)> = uplink
        .reader()
        .prepare("SELECT external_id, name FROM label WHERE kind = 'folder' ORDER BY 1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // Stalwart's special-use folders, an Archive to be made, and Projects.
    assert_eq!(
        folders,
        [
            ("ARCHIVE", "ARCHIVE"),
            ("DRAFT", "DRAFT"),
            ("INBOX", "INBOX"),
            ("SENT", "SENT"),
            ("SPAM", "SPAM"),
            ("TRASH", "TRASH"),
            ("mailbox:Projects", "Projects"),
        ]
        .map(|(id, name)| (id.to_owned(), name.to_owned()))
    );
    append(&mut other, "INBOX", &message("second", "a quixotic body")).await;
    let second = uplink.arrived("second").await;

    // Archiving moves the message to an Archive, made for it; it is the
    // same entry there. Undo moves it back.
    let in_inbox = uplink.external_id(first);
    let archived = dispatched(
        uplink
            .mail
            .modify_labels(vec![first], vec![], vec![uplink.label("INBOX")])
            .await
            .unwrap(),
    );
    eventually("the archive to reach the server", || async {
        find(&mut stalwart.client("alice").await, "Archive", "first")
            .await
            .map(drop)
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first)
            == [
                ("ARCHIVE".into(), true, true),
                ("UNREAD".into(), true, true),
            ]
            && uplink.external_id(first) != in_inbox)
            .then_some(())
    })
    .await;
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach the server", || async {
        find(&mut stalwart.client("alice").await, "INBOX", "first")
            .await
            .map(drop)
    })
    .await;
    eventually("the undo to be observed", || async {
        (label_set(&uplink.labels(first)) == ["INBOX", "UNREAD"]).then_some(())
    })
    .await;

    // Starring and reading flag the message and mark it read.
    uplink
        .mail
        .modify_labels(
            vec![second],
            vec![uplink.label("STARRED")],
            vec![uplink.label("UNREAD")],
        )
        .await
        .unwrap();
    eventually("the star to reach the server", || async {
        let (_, flags) = find(&mut stalwart.client("alice").await, "INBOX", "second").await?;
        (flags.contains(&"Seen".into()) && flags.contains(&"Flagged".into())).then_some(())
    })
    .await;

    // Changes another client makes show up in uplink: a move and a
    // keyword, the entry the same; then an expunge.
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(second)) == ["INBOX", "STARRED"]).then_some(())
    })
    .await;
    let (uid, _) = find(&mut other, "INBOX", "second").await.unwrap();
    store(&mut other, "INBOX", uid, "+FLAGS (Red)").await;
    other
        .session
        .uid_mv(uid.to_string(), "Projects")
        .await
        .unwrap();
    eventually("the move to be observed", || async {
        (label_set(&uplink.labels(second)) == ["STARRED", "kw:Red", "mailbox:Projects"])
            .then_some(())
    })
    .await;
    let (uid, _) = find(&mut other, "INBOX", "first").await.unwrap();
    store(&mut other, "INBOX", uid, "+FLAGS (\\Deleted)").await;
    let _: Vec<_> = other
        .session
        .expunge()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    eventually("the expunge to be observed", || async {
        uplink.entry("first").is_none().then_some(())
    })
    .await;

    // Removing a keyword in uplink removes it at the server.
    uplink
        .mail
        .modify_labels(vec![second], vec![], vec![uplink.label("kw:Red")])
        .await
        .unwrap();
    eventually("the keyword's removal to reach the server", || async {
        let (_, flags) = find(&mut stalwart.client("alice").await, "Projects", "second").await?;
        (!flags.iter().any(|f| f.contains("Red"))).then_some(())
    })
    .await;
    eventually("the keyword's removal to be observed", || async {
        (!label_set(&uplink.labels(second)).contains(&"kw:Red")).then_some(())
    })
    .await;

    // A restarted sync resumes from the recorded state: idle polls record
    // nothing, and new mail still arrives.
    let settled = uplink.settled().await;
    sync.abort();
    let _ = sync.await;
    let sync = uplink.start(provider(&stalwart), ALICE);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(uplink.log_len(), settled, "the restarted sync re-synced");
    append(&mut other, "INBOX", &message("third", "after restart")).await;
    uplink.arrived("third").await;
    sync.abort();
}

/// Polling once an hour, uplink still sees what reaches the Inbox at
/// once, as the server tells of it by IDLE.
#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_is_watched_by_idle() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let sync = uplink.start_polling(provider(&stalwart), ALICE, Duration::from_secs(3600));
    eventually("the first sync", || async { uplink.synced().then_some(()) }).await;

    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("pushed", "")).await;
    let pushed = uplink.arrived("pushed").await;
    let (uid, _) = find(&mut other, "INBOX", "pushed").await.unwrap();
    store(&mut other, "INBOX", uid, "+FLAGS (\\Flagged)").await;
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(pushed)) == ["INBOX", "STARRED", "UNREAD"]).then_some(())
    })
    .await;
    sync.abort();
}

/// On a server without NOTIFY, a folder the account names is watched by
/// IDLE too, on a connection of its own; one it does not name waits for
/// the poll.
#[tokio::test(flavor = "multi_thread")]
async fn named_folders_are_watched_by_idle() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    for folder in ["Alerts", "Lists"] {
        other.session.create(folder).await.unwrap();
    }
    let account = ImapAccountConfig {
        watch: vec!["Alerts".into()],
        ..stalwart.account("alice")
    };
    let sign_in = stalwart.sign_in(account.login());
    let provider = ImapSync::new(account, sign_in, stalwart.tls.clone());
    let sync = uplink.start_polling(provider, ALICE, Duration::from_secs(3600));
    eventually("the first sync", || async { uplink.synced().then_some(()) }).await;

    // Mail to a folder unwatched, then to the one watched: the wake for the
    // latter finds both.
    append(&mut other, "Lists", &message("unwatched", "")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(uplink.entry("unwatched"), None, "Lists is not watched");
    append(&mut other, "Alerts", &message("alert", "")).await;
    let alert = uplink.arrived("alert").await;
    uplink.arrived("unwatched").await;
    let (uid, _) = find(&mut other, "Alerts", "alert").await.unwrap();
    store(&mut other, "Alerts", uid, "+FLAGS (\\Seen)").await;
    eventually("the read to be observed", || async {
        (label_set(&uplink.labels(alert)) == ["mailbox:Alerts"]).then_some(())
    })
    .await;
    sync.abort();
}

/// Polled, a change in the folder the sync last selected is seen: of it, a
/// STATUS may answer as the connection last saw it (Dovecot does).
#[tokio::test(flavor = "multi_thread")]
async fn a_change_in_the_folder_left_selected_is_seen() {
    let uplink = Uplink::new(MailProvider::Imap);
    let dovecot = Dovecot::start(uplink.dir.path()).await;
    let account = dovecot.account();
    let provider = ImapSync::new(
        account.clone(),
        dovecot.sign_in(&account),
        dovecot.tls.clone(),
    );
    let mut other = dovecot.client().await;
    // In every folder, so that whichever the sync looks at last is
    // selected as it polls.
    for folder in ["INBOX", "Alerts"] {
        append(&mut other, folder, &message(folder, "")).await;
    }
    let sync = uplink.start(provider, ALICE);
    for folder in ["INBOX", "Alerts"] {
        let entry = uplink.arrived(folder).await;
        let (uid, _) = find(&mut other, folder, folder).await.unwrap();
        store(&mut other, folder, uid, "+FLAGS (\\Seen)").await;
        eventually(&format!("the read in {folder} to be observed"), || async {
            (!uplink.labels(entry).iter().any(|(l, ..)| l == "UNREAD")).then_some(())
        })
        .await;
    }
    sync.abort();
}

/// On a server with NOTIFY, every folder is watched, on one connection.
#[tokio::test(flavor = "multi_thread")]
async fn every_folder_is_watched_by_notify() {
    let uplink = Uplink::new(MailProvider::Imap);
    let dovecot = Dovecot::start(uplink.dir.path()).await;
    let account = dovecot.account();
    let provider = ImapSync::new(
        account.clone(),
        dovecot.sign_in(&account),
        dovecot.tls.clone(),
    );
    let sync = uplink.start_polling(provider, ALICE, Duration::from_secs(3600));
    eventually("the first sync", || async { uplink.synced().then_some(()) }).await;

    let mut other = dovecot.client().await;
    assert!(other.notify, "Dovecot has NOTIFY");
    append(&mut other, "Alerts", &message("alert", "")).await;
    let alert = uplink.arrived("alert").await;
    let (uid, _) = find(&mut other, "Alerts", "alert").await.unwrap();
    store(&mut other, "Alerts", uid, "+FLAGS (\\Flagged)").await;
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(alert)) == ["STARRED", "UNREAD", "mailbox:Alerts"]).then_some(())
    })
    .await;
    // A folder made since, as well.
    other.session.create("Later").await.unwrap();
    append(&mut other, "Later", &message("later", "")).await;
    uplink.arrived("later").await;
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_lists_the_mailbox_in_steps() {
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    for i in 0..120 {
        append(&mut other, "INBOX", &message(&format!("m{i:03}"), "")).await;
    }
    let sync = uplink.start(provider(&stalwart), ALICE);
    eventually("the listing to be done", || async {
        uplink.synced().then_some(())
    })
    .await;
    let entries: i64 = uplink
        .reader()
        .query_row("SELECT count(*) FROM mail_entry", [], |r| r.get(0))
        .unwrap();
    assert_eq!(entries, 120);
    // In steps, each a log entry.
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
    let uplink = Uplink::new(MailProvider::Imap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let mut other = stalwart.client("alice").await;
    append(&mut other, "INBOX", &message("original", "lunch?")).await;
    let sync = uplink.start(provider(&stalwart), ALICE);
    let original = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original);

    // A reply to Bob: delivered with uplink's Message-ID, and synced back
    // from Sent as the same message, in the original's conversation.
    let reply = format!("reply {}", common::nonce());
    uplink.send(BOB, &reply, Some(original_message)).await;
    let (uid, _) = eventually("the reply to reach Bob", || async {
        find(&mut stalwart.client("bob").await, "INBOX", "reply").await
    })
    .await;
    let mut bob = stalwart.client("bob").await;
    bob.session.select("INBOX").await.unwrap();
    let fetched: Vec<_> = bob
        .session
        .uid_fetch(
            uid.to_string(),
            "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO)]",
        )
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let headers = String::from_utf8(fetched[0].header().unwrap().to_vec()).unwrap();
    let sent = eventually("the reply's copy to be synced", || async {
        uplink.entry("reply")
    })
    .await;
    assert_eq!(label_set(&uplink.labels(sent)), ["SENT"]);
    let (_, message_id, thread) = uplink.message_of(sent);
    assert!(headers.contains(&format!("<{message_id}>")), "{headers}");
    assert!(message_id.ends_with("@test.uplink"), "{message_id}");
    assert_eq!(thread, original_thread);
    assert!(headers.contains("In-Reply-To: <original."), "{headers}");
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    sync.abort();
}

/// An account that signs in by XOAUTH2, emulate's Google its OAuth
/// provider: authorized as `uplink oauth init` does it, by the account's
/// own `oauth` settings, the browser played on emulate's sign-in form.
/// The access token uplink redeems the refresh token for is what Stalwart
/// checks, at the provider's userinfo endpoint, for IMAP and SMTP alike.
#[tokio::test(flavor = "multi_thread")]
async fn signs_in_by_xoauth2() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "google:\n  users:\n    - email: {ALICE}\n      name: Alice\n  oauth_clients:\n    \
         - client_id: uplink-test\n      client_secret: uplink-test-secret\n      \
         redirect_uris: [\"http://127.0.0.1/\"]\n"
    );
    let (base, _emulate) = common::start_emulate(dir.path(), "google", &config).await;
    let userinfo = format!("{base}/oauth2/v2/userinfo");
    let stalwart = Stalwart::start_with(
        dir.path(),
        Directory::OpenId {
            userinfo: &userinfo,
        },
    )
    .await;
    let account = ImapAccountConfig {
        user: None,
        oauth: Some(OAuthConfig {
            client_id: "uplink-test".into(),
            client_secret: Some("uplink-test-secret".into()),
            auth_url: format!("{base}/o/oauth2/v2/auth"),
            token_url: format!("{base}/oauth2/token"),
            scope: "openid email".into(),
            token_file: stalwart.dir.join("alice.token"),
        }),
        ..stalwart.account("alice")
    };

    let http = reqwest::Client::new();
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let (authorized, ()) = tokio::join!(
        sign_in::authorize(
            account.login(),
            &stalwart.key,
            |url| url_tx.send(url.clone()).unwrap(),
            std::future::pending(),
        ),
        async { common::sign_in_at_emulate(&http, &base, url_rx.await.unwrap(), ALICE).await },
    );
    authorized.unwrap();

    let sign_in = stalwart.sign_in(account.login());
    assert!(matches!(sign_in, SignIn::OAuth(_)));
    // Stalwart makes Alice's account as she first signs in.
    session::connect(&account, &sign_in, &stalwart.tls)
        .await
        .unwrap();
    let raw = format!(
        "Message-ID: <oauth@uplink.test>\r\nFrom: {ALICE}\r\nTo: {ALICE}\r\n\
         Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: oauth 1\r\n\r\nhi\r\n"
    );
    smtp::send(
        account.login(),
        &account.smtp,
        &sign_in,
        &stalwart.tls,
        raw.as_bytes(),
    )
    .await
    .unwrap();
    eventually("the message to arrive", || async {
        let mut alice = session::connect(&account, &sign_in, &stalwart.tls)
            .await
            .unwrap();
        find(&mut alice, "INBOX", "oauth").await
    })
    .await;
}
