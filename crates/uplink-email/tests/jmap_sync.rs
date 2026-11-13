//! JMAP sync against Stalwart (`UPLINK_STALWART`, set by the dev shell and
//! `nix flake check`), a real mail server, one per test. The other side is
//! played as another mail client would, over JMAP.

mod common;

use common::{
    eventually, message,
    stalwart::{Stalwart, ALICE, BOB},
    uplink::{label_set, Uplink},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use uplink_email::{
    account::{Outgoing, Provider},
    jmap::{
        api::{Got, Jmap, Queried, Request, Set},
        sync::JmapSync,
    },
    mail_server::smtp,
};
use uplink_protocol::{compose::OutboxId, mail::MailProvider};

/// uplink's side of Alice's account.
fn provider(stalwart: &Stalwart) -> JmapSync {
    let account = stalwart.jmap_account("alice");
    JmapSync::new(account.id.clone(), stalwart.jmap_of(&account))
}

/// Makes one call; its response.
async fn call<T: serde::de::DeserializeOwned>(jmap: &Jmap, method: &str, arguments: Value) -> T {
    let mut request = Request::default();
    let c = request.call(method, arguments);
    jmap.call(request).await.unwrap().take(c).unwrap()
}

/// The mailboxes, by role, else by name.
async fn mailboxes(jmap: &Jmap) -> BTreeMap<String, String> {
    let got: Got<Value> = call(jmap, "Mailbox/get", json!({ "ids": null })).await;
    got.list
        .into_iter()
        .map(|m| {
            let key = m["role"]
                .as_str()
                .or(m["name"].as_str())
                .unwrap()
                .to_owned();
            (key, m["id"].as_str().unwrap().to_owned())
        })
        .collect()
}

async fn create_mailbox(jmap: &Jmap, name: &str) -> String {
    let set: Set = call(
        jmap,
        "Mailbox/set",
        json!({ "create": { "new": { "name": name, "parentId": null } } }),
    )
    .await;
    set.created["new"]["id"].as_str().unwrap().to_owned()
}

/// Delivers `raw` to the mailbox of `role`, as another client would file
/// it; its id.
async fn import(jmap: &Jmap, role: &str, raw: &[u8]) -> String {
    let mailbox = mailboxes(jmap).await[role].clone();
    let blob = jmap.upload(raw, "message/rfc822").await.unwrap();
    let imported: Value = call(
        jmap,
        "Email/import",
        json!({ "emails": { "m": { "blobId": blob, "mailboxIds": { mailbox: true } } } }),
    )
    .await;
    imported["created"]["m"]["id"].as_str().unwrap().to_owned()
}

/// The email with `tag` in its subject, if there is one: its id, and its
/// mailboxes' roles (else names) and keywords.
async fn find(jmap: &Jmap, tag: &str) -> Option<(String, BTreeSet<String>, BTreeSet<String>)> {
    let queried: Queried = call(
        jmap,
        "Email/query",
        json!({ "filter": { "subject": format!("{tag} ") } }),
    )
    .await;
    let id = queried.ids.first()?.clone();
    let got: Got<Value> = call(
        jmap,
        "Email/get",
        json!({ "ids": [id], "properties": ["mailboxIds", "keywords"] }),
    )
    .await;
    let names: BTreeMap<String, String> = mailboxes(jmap)
        .await
        .into_iter()
        .map(|(k, v)| (v, k))
        .collect();
    let email = &got.list[0];
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    Some((
        id,
        keys(&email["mailboxIds"])
            .into_iter()
            .map(|m| names[&m].clone())
            .collect(),
        keys(&email["keywords"]).into_iter().collect(),
    ))
}

async fn update(jmap: &Jmap, id: &str, patch: Value) {
    let set: Set = call(jmap, "Email/set", json!({ "update": { id: patch } })).await;
    assert!(set.not_updated.is_empty(), "{:?}", set.not_updated);
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_the_mailboxes_as_labels_and_pushes_changes() {
    let uplink = Uplink::new(MailProvider::Jmap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let other = stalwart.jmap("alice");
    let projects = create_mailbox(&other, "Projects").await;
    let projects_label = format!("mailbox:{projects}");

    // Mail present before the first sync, and mail arriving afterwards.
    import(&other, "inbox", &message("first", "hello world")).await;
    let sync = uplink.start(provider(&stalwart), ALICE);
    let first = uplink.arrived("first").await;
    assert_eq!(
        uplink.labels(first),
        [("INBOX".into(), true, true), ("UNREAD".into(), true, true)]
    );
    let labels: Vec<(String, String, bool)> = uplink
        .reader()
        .prepare("SELECT external_id, name, folder FROM label ORDER BY 1")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // Stalwart's mailboxes of a role, Projects, and the state labels; none
    // a folder, and no Archive, which is made as needed.
    let mut expected: Vec<(String, String, bool)> = [
        "DRAFT", "INBOX", "SENT", "SPAM", "STARRED", "TRASH", "UNREAD",
    ]
    .map(|l| (l.to_owned(), l.to_owned(), false))
    .into();
    expected.push((projects_label.clone(), "Projects".into(), false));
    expected.sort();
    assert_eq!(labels, expected);
    import(&other, "inbox", &message("second", "a quixotic body")).await;
    let second = uplink.arrived("second").await;

    // Archiving takes the email out of the Inbox, and, as it is then in
    // no mailbox, into an Archive, made for it; which uplink does not
    // show. It is the same entry. Undo takes it back, out of the Archive.
    let id = uplink.external_id(first);
    let archived = uplink
        .mail
        .modify_labels(vec![first], vec![], vec![uplink.label("INBOX")])
        .await
        .unwrap();
    let uplink_protocol::ServerResponse::Dispatched {
        tx_log_id: archived,
    } = archived
    else {
        panic!("{archived:?}")
    };
    eventually("the archive to reach the server", || async {
        let (_, mailboxes, _) = find(&other, "first").await?;
        (mailboxes == set(&["archive"])).then_some(())
    })
    .await;
    eventually("the archive to be observed", || async {
        (uplink.labels(first) == [("UNREAD".into(), true, true)]).then_some(())
    })
    .await;
    assert_eq!(uplink.external_id(first), id);
    uplink.dispatcher.undo(archived).await.unwrap();
    eventually("the undo to reach the server", || async {
        let (_, mailboxes, _) = find(&other, "first").await?;
        (mailboxes == set(&["inbox"])).then_some(())
    })
    .await;

    // Starring and reading set keywords; a label is a mailbox more.
    uplink
        .mail
        .modify_labels(
            vec![second],
            vec![uplink.label("STARRED"), uplink.label(&projects_label)],
            vec![uplink.label("UNREAD")],
        )
        .await
        .unwrap();
    eventually("the changes to reach the server", || async {
        let (_, mailboxes, keywords) = find(&other, "second").await?;
        (mailboxes == set(&["Projects", "inbox"]) && keywords == set(&["$flagged", "$seen"]))
            .then_some(())
    })
    .await;
    eventually("the changes to be observed", || async {
        (label_set(&uplink.labels(second)) == ["INBOX", "STARRED", &projects_label]).then_some(())
    })
    .await;

    // Changes another client makes show up in uplink: a keyword, a
    // mailbox left; then the email destroyed.
    let (second_id, ..) = find(&other, "second").await.unwrap();
    let inbox = mailboxes(&other).await["inbox"].clone();
    update(
        &other,
        &second_id,
        json!({ "keywords/red": true, format!("mailboxIds/{inbox}"): null }),
    )
    .await;
    eventually("the other client's changes to be observed", || async {
        (label_set(&uplink.labels(second)) == ["STARRED", "kw:red", &projects_label]).then_some(())
    })
    .await;
    let (first_id, ..) = find(&other, "first").await.unwrap();
    let _: Set = call(&other, "Email/set", json!({ "destroy": [first_id] })).await;
    eventually("the destruction to be observed", || async {
        uplink.entry("first").is_none().then_some(())
    })
    .await;

    // Removing a keyword in uplink removes it at the server.
    uplink
        .mail
        .modify_labels(vec![second], vec![], vec![uplink.label("kw:red")])
        .await
        .unwrap();
    eventually("the keyword's removal to reach the server", || async {
        let (_, _, keywords) = find(&other, "second").await?;
        (!keywords.contains("red")).then_some(())
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
    import(&other, "inbox", &message("third", "after restart")).await;
    uplink.arrived("third").await;
    sync.abort();
}

/// Polling once an hour, uplink still sees changes at once, as the server
/// tells of them by its event source.
#[tokio::test(flavor = "multi_thread")]
async fn changes_are_told_by_the_event_source() {
    let uplink = Uplink::new(MailProvider::Jmap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let sync = uplink.start_polling(provider(&stalwart), ALICE, Duration::from_secs(3600));
    eventually("the first sync", || async { uplink.synced().then_some(()) }).await;

    // Delivered by SMTP, as mail arrives (Stalwart tells nothing of an
    // `Email/import`).
    let bob = stalwart.account("bob");
    smtp::send(
        bob.login(),
        &bob.smtp,
        &stalwart.sign_in(bob.login()),
        &stalwart.tls,
        &message("pushed", ""),
    )
    .await
    .unwrap();
    let pushed = uplink.arrived("pushed").await;
    let other = stalwart.jmap("alice");
    let (id, ..) = find(&other, "pushed").await.unwrap();
    update(&other, &id, json!({ "keywords/$flagged": true })).await;
    eventually("the star to be observed", || async {
        (label_set(&uplink.labels(pushed)) == ["INBOX", "STARRED", "UNREAD"]).then_some(())
    })
    .await;
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_lists_the_emails_in_steps() {
    let uplink = Uplink::new(MailProvider::Jmap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let other = stalwart.jmap("alice");
    for i in 0..120 {
        import(&other, "inbox", &message(&format!("m{i:03}"), "")).await;
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
    // In steps, each a log entry: the listing begun, the page's emails
    // fetched in three, and its end.
    let steps: i64 = uplink
        .reader()
        .query_row(
            "SELECT count(*) FROM tx_log WHERE op_type = 'mail.observed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(steps, 5);
    sync.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_by_submission_and_files_the_copy_in_sent() {
    let uplink = Uplink::new(MailProvider::Jmap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let other = stalwart.jmap("alice");
    import(&other, "inbox", &message("original", "lunch?")).await;
    let sync = uplink.start(provider(&stalwart), ALICE);
    let original = uplink.arrived("original").await;
    let (original_message, _, original_thread) = uplink.message_of(original);
    let subject: String = uplink
        .reader()
        .query_row(
            "SELECT subject FROM message WHERE id = ?1",
            [original_message.0],
            |r| r.get(0),
        )
        .unwrap();

    // A reply to Bob: delivered with uplink's Message-ID, and synced back
    // from Sent as the same message, in the original's conversation (the
    // server's: Stalwart's threads, as Gmail's, are of a subject too).
    uplink
        .send(BOB, &format!("Re: {subject}"), Some(original_message))
        .await;
    let bob = stalwart.jmap("bob");
    // Bob has the reply only.
    let (delivered, ..) = eventually("the reply to reach Bob", || async {
        find(&bob, "original").await
    })
    .await;
    let got: Got<Value> = call(
        &bob,
        "Email/get",
        json!({ "ids": [delivered], "properties": ["messageId", "inReplyTo"] }),
    )
    .await;
    let sent = eventually("the reply's copy to be synced", || async {
        uplink.entry("Re:")
    })
    .await;
    eventually("the copy to be in Sent", || async {
        (label_set(&uplink.labels(sent)) == ["SENT"]).then_some(())
    })
    .await;
    let (_, message_id, thread) = uplink.message_of(sent);
    assert_eq!(got.list[0]["messageId"], json!([message_id]));
    assert!(message_id.ends_with("@test.uplink"), "{message_id}");
    assert_eq!(thread, original_thread);
    let in_reply_to = got.list[0]["inReplyTo"][0].as_str().unwrap();
    assert!(in_reply_to.starts_with("original."), "{in_reply_to}");
    eventually("the outbox to empty", || async {
        uplink.outbox().await.is_empty().then_some(())
    })
    .await;
    sync.abort();
}

/// A send tried again, its first attempt's outcome unknown: its draft,
/// submitted already, is not submitted twice; Bcc goes to its recipient,
/// but not in what is delivered.
#[tokio::test(flavor = "multi_thread")]
async fn a_draft_submitted_is_not_submitted_again() {
    let uplink = Uplink::new(MailProvider::Jmap);
    let stalwart = Stalwart::start(uplink.dir.path()).await;
    let jmap = provider(&stalwart);
    let other = stalwart.jmap("alice");
    let raw = format!(
        "Message-ID: <once@uplink.test>\r\nFrom: {ALICE}\r\nTo: {ALICE}\r\nBcc: {BOB}\r\n\
         Date: Tue, 3 Nov 2026 10:00:00 +0000\r\nSubject: once 1\r\n\r\nhi\r\n"
    );
    // Drafted, then submitted as another attempt would have.
    let draft = import(&other, "drafts", raw.as_bytes()).await;
    let identity: Got<Value> = call(&other, "Identity/get", json!({ "ids": null })).await;
    let submitted: Set = call(
        &other,
        "EmailSubmission/set",
        json!({ "create": { "s": { "identityId": identity.list[0]["id"], "emailId": draft } } }),
    )
    .await;
    assert!(submitted.created.contains_key("s"), "{submitted:?}");

    let account = stalwart.jmap_account("alice");
    let task = uplink_email::account::SyncTask::new(
        jmap,
        "test".into(),
        account.address.clone(),
        uplink.dispatcher.clone(),
        uplink.reader(),
        uplink.dir.path(),
        Duration::from_secs(3600),
    );
    let copy = task
        .provider
        .send(
            &task.account,
            Outgoing {
                item: OutboxId(1),
                raw: raw.as_bytes(),
                message_id: "once@uplink.test",
                not_before: 0,
                thread: None,
                provider_draft: Some(&draft),
                provider_thread: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(copy, uplink_email::compose::SentCopy::Id(draft.clone()));
    let bob = stalwart.jmap("bob");
    let (delivered, ..) = eventually("the mail to reach Bob", || async {
        find(&bob, "once").await
    })
    .await;
    let got: Got<Value> = call(
        &bob,
        "Email/get",
        json!({ "ids": [delivered], "properties": ["bcc"] }),
    )
    .await;
    assert_eq!(got.list[0]["bcc"], Value::Null);
    let queried: Queried = call(&bob, "Email/query", json!({})).await;
    assert_eq!(queried.ids.len(), 1, "delivered twice");
    let submissions: Queried = call(
        &other,
        "EmailSubmission/query",
        json!({ "filter": { "emailIds": [draft] } }),
    )
    .await;
    assert_eq!(submissions.ids.len(), 1, "submitted twice");
}
