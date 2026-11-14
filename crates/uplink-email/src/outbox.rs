//! The outbox: what of the mail accounts is not yet reconciled with their
//! providers (SPEC §6.6). Label changes pending, by the op that made them;
//! mail being sent; and why an account's changes are not reaching it.

use crate::{
    compose,
    mailbox::{decision_reason, LabelChange},
};
use anyhow::Result;
use std::collections::BTreeMap;
use uplink_protocol::{
    compose::{AccountOutbox, Failing, OutboxItem, Pending, PendingChanges, PendingOp, Refusal},
    mail::{EntryId, LabelId},
    TxLogId,
};
use uplink_storage::rusqlite::{params, Transaction};

/// What of a mail account is not yet reconciled.
#[derive(Default)]
struct MailOutbox {
    failing: Option<Failing>,
    changes: Vec<PendingChanges>,
    sends: Vec<OutboxItem>,
}

/// Each mail account with anything not yet reconciled, in id order.
pub fn outbox(tx: &Transaction) -> Result<Vec<AccountOutbox>> {
    let mut accounts: BTreeMap<String, MailOutbox> = BTreeMap::new();
    fn account(accounts: &mut BTreeMap<String, MailOutbox>, id: String) -> &mut MailOutbox {
        accounts.entry(id).or_default()
    }
    for (id, failing) in uplink_tx_log::push::failing(tx, &crate::KINDS)? {
        account(&mut accounts, id).failing = Some(failing);
    }
    // Per account and op, newest first, the entries pending and those
    // waiting for a decision.
    struct Group {
        account: String,
        by: i64,
        op: PendingOp,
        decision: Option<String>,
        entries: u32,
    }
    let groups: Vec<Group> = tx
        .prepare(
            "SELECT e.account_id, el.desired_by, t.description, t.ts, el.decision,
                    count(DISTINCT el.entry)
             FROM entry_label el
             JOIN mail_entry e ON e.id = el.entry
             JOIN tx_log t ON t.id = el.desired_by
             GROUP BY e.account_id, el.desired_by, el.decision
             ORDER BY e.account_id, el.desired_by DESC,
                      el.decision IS NOT NULL, el.decision",
        )?
        .query_map([], |r| {
            let by: i64 = r.get(1)?;
            Ok(Group {
                account: r.get(0)?,
                by,
                op: PendingOp {
                    id: TxLogId(by),
                    description: r.get(2)?,
                    ts: r.get(3)?,
                },
                decision: r.get(4)?,
                entries: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let entries = entries_pending(tx)?;
    for group in groups {
        let pending = entries[&(group.account.clone(), group.by)];
        let outbox = account(&mut accounts, group.account);
        let op = Some(group.op);
        let changes = match outbox.changes.last_mut() {
            Some(last) if last.op == op => last,
            _ => {
                outbox.changes.push(PendingChanges {
                    op,
                    entries: pending,
                    refused: vec![],
                });
                outbox.changes.last_mut().expect("just pushed")
            }
        };
        if let Some(decision) = group.decision {
            changes.refused.push(Refusal {
                reason: decision_reason(&decision)?,
                entries: group.entries,
            });
        }
    }
    for item in compose::outbox(tx)? {
        account(&mut accounts, item.account.clone())
            .sends
            .push(item);
    }
    Ok(accounts
        .into_iter()
        .map(|(account, outbox)| AccountOutbox {
            account,
            failing: outbox.failing,
            pending: Pending::Mail {
                changes: outbox.changes,
                sends: outbox.sends,
            },
        })
        .collect())
}

/// Per account and op, the entries with changes pending: an entry with
/// some refused and some not counts once.
fn entries_pending(tx: &Transaction) -> Result<BTreeMap<(String, i64), u32>> {
    Ok(tx
        .prepare(
            "SELECT e.account_id, el.desired_by, count(DISTINCT el.entry)
             FROM entry_label el JOIN mail_entry e ON e.id = el.entry
             WHERE el.desired_by IS NOT NULL
             GROUP BY e.account_id, el.desired_by",
        )?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

/// The changes that would take `op`'s changes waiting for a decision in
/// `account` back to what the provider has.
pub(crate) fn keeping_observed(
    tx: &Transaction,
    account: &str,
    op: Option<TxLogId>,
) -> Result<Vec<LabelChange>> {
    Ok(tx
        .prepare(
            "SELECT el.entry, el.label, el.observed
             FROM entry_label el JOIN mail_entry e ON e.id = el.entry
             WHERE e.account_id = ?1 AND el.desired_by IS ?2
               AND el.decision IS NOT NULL AND el.desired != el.observed
             ORDER BY el.entry, el.label",
        )?
        .query_map(params![account, op.map(|o| o.0)], |r| {
            Ok(LabelChange {
                entry: EntryId(r.get(0)?),
                label: LabelId(r.get(1)?),
                desired: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}
