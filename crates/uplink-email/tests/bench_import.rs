//! The first sync of a real mailbox, and a bulk change pushed to it,
//! timed: run by hand, `--ignored`, with `UPLINK_BENCH_MAILDIR` naming a
//! Maildir++ (its `cur`, and folders `.<name>/cur`) that Dovecot serves a
//! copy of. Prints a JSON line per measurement, and to stderr the labels
//! synced and the log entries the push recorded.
//!
//! `UPLINK_BENCH_MAILDIR=… cargo test --release -p uplink-email --test
//! bench_import -- --ignored --nocapture`

mod common;

use common::{dovecot::Dovecot, uplink::Uplink};
use std::time::{Duration, Instant};
use uplink_email::imap::sync::ImapSync;
use uplink_protocol::mail::{EntryId, MailProvider};

fn report(what: &str, since: Instant, count: i64) {
    println!(
        "{}",
        serde_json::json!({
            "what": what,
            "ms": since.elapsed().as_secs_f64() * 1000.0,
            "count": count,
        })
    );
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3600);
    while !done() {
        assert!(Instant::now() < deadline, "{what} took over an hour");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn first_sync_of_a_real_mailbox() {
    let source = std::env::var("UPLINK_BENCH_MAILDIR").expect("UPLINK_BENCH_MAILDIR");
    let uplink = Uplink::new(MailProvider::Imap);
    let dovecot = Dovecot::start(uplink.dir.path()).await;
    let maildir = uplink.dir.path().join("dovecot/home/alice/Maildir");
    std::fs::create_dir_all(&maildir).unwrap();
    let copied = std::process::Command::new("cp")
        .args(["-r", "--reflink=auto"])
        .arg(format!("{source}/."))
        .arg(&maildir)
        .status()
        .unwrap();
    assert!(copied.success());
    let account = dovecot.account();
    let provider = ImapSync::new(
        account.clone(),
        dovecot.sign_in(&account),
        dovecot.tls.clone(),
    );

    let start = Instant::now();
    let sync = uplink.start_polling(provider, "alice@example.com", Duration::from_secs(3600));
    until("the first sync", || uplink.synced()).await;
    report("first sync", start, uplink.entries());
    let folders = || -> Vec<(String, i64)> {
        uplink
            .reader()
            .prepare(
                "SELECT l.name, count(*) FROM entry_label el JOIN label l ON l.id = el.label
                 WHERE el.desired GROUP BY l.id ORDER BY l.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    eprintln!("synced: {:?}", folders());

    // Every unread message read, as one op, as "select all" and "mark
    // read" do; then until the server has each.
    let unread = uplink.label("UNREAD");
    let entries: Vec<EntryId> = uplink
        .reader()
        .prepare("SELECT entry FROM entry_label WHERE label = ?1 AND desired")
        .unwrap()
        .query_map([unread.0], |r| Ok(EntryId(r.get(0)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let count = entries.len() as i64;
    let pending = || -> i64 {
        uplink
            .reader()
            .query_row(
                "SELECT count(*) FROM entry_label WHERE desired_by IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let logged = || -> i64 {
        uplink
            .reader()
            .query_row("SELECT count(*) FROM tx_log", [], |r| r.get(0))
            .unwrap()
    };
    let start = Instant::now();
    uplink
        .mail
        .modify_labels(entries, vec![], vec![unread])
        .await
        .unwrap();
    report("mark read: applied", start, count);
    let before = logged();
    until("the reads to be pushed", || pending() == 0).await;
    report("mark read: pushed and confirmed", start, count);
    let kinds: Vec<(String, i64)> = uplink
        .reader()
        .prepare("SELECT op_type, count(*) FROM tx_log WHERE id > ?1 GROUP BY 1 ORDER BY 2 DESC")
        .unwrap()
        .query_map([before], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    eprintln!("logged while pushing: {} {kinds:?}", logged() - before);
    sync.abort();
}
