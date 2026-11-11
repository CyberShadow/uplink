//! The outbox: what of the mail accounts is not yet reconciled with their
//! providers (SPEC §6.6). Label changes pending, by the op that made them;
//! mail being sent; and why an account's changes are not reaching it.

use crate::{compose, mailbox::LabelChange};
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
    let failing: Vec<(String, String, i64)> = tx
        .prepare(
            "SELECT id, push_error, push_failing_since FROM account
             WHERE push_error IS NOT NULL AND kind IN (?1, ?2)",
        )?
        .query_map([crate::gmail::sync::KIND, crate::m365::sync::KIND], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    for (id, error, since) in failing {
        account(&mut accounts, id).failing = Some(Failing { error, since });
    }
    // Per account and op, newest first (changes of no op last), the
    // entries pending and refused.
    struct Group {
        account: String,
        by: Option<i64>,
        op: Option<PendingOp>,
        refused: Option<String>,
        entries: u32,
    }
    let groups: Vec<Group> = tx
        .prepare(
            "SELECT e.account_id, el.desired_by, t.description, t.ts, el.refused,
                    count(DISTINCT el.entry)
             FROM entry_label el
             JOIN mail_entry e ON e.id = el.entry
             LEFT JOIN tx_log t ON t.id = el.desired_by
             WHERE el.desired != el.observed
             GROUP BY e.account_id, el.desired_by, el.refused
             ORDER BY e.account_id, el.desired_by IS NULL, el.desired_by DESC,
                      el.refused IS NOT NULL, el.refused",
        )?
        .query_map([], |r| {
            let by: Option<i64> = r.get(1)?;
            let op = match by {
                Some(id) => Some(PendingOp {
                    id: TxLogId(id),
                    description: r.get(2)?,
                    ts: r.get(3)?,
                }),
                None => None,
            };
            Ok(Group {
                account: r.get(0)?,
                by,
                op,
                refused: r.get(4)?,
                entries: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let entries = entries_pending(tx)?;
    for group in groups {
        let pending = entries[&(group.account.clone(), group.by)];
        let outbox = account(&mut accounts, group.account);
        let changes = match outbox.changes.last_mut() {
            Some(last) if last.op == group.op => last,
            _ => {
                outbox.changes.push(PendingChanges {
                    op: group.op,
                    entries: pending,
                    refused: vec![],
                });
                outbox.changes.last_mut().expect("just pushed")
            }
        };
        if let Some(reason) = group.refused {
            changes.refused.push(Refusal {
                reason,
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
fn entries_pending(tx: &Transaction) -> Result<BTreeMap<(String, Option<i64>), u32>> {
    Ok(tx
        .prepare(
            "SELECT e.account_id, el.desired_by, count(DISTINCT el.entry)
             FROM entry_label el JOIN mail_entry e ON e.id = el.entry
             WHERE el.desired != el.observed
             GROUP BY e.account_id, el.desired_by",
        )?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect::<Result<_, _>>()?)
}

/// The changes that would take `op`'s refused changes in `account` (those
/// of no op, if `None`) back to what the provider has.
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
               AND el.refused IS NOT NULL AND el.desired != el.observed
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
