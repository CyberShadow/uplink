//! The user's rules that have mail's remote images shown unasked (SPEC
//! §8.9): all of a sender's, by address or domain, or those at a host,
//! whoever's. Only uplink keeps them: no provider has them to mirror.
//! Likely tracking pixels never load, whatever the rules.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{
    mail::{ImageRule, ImageRuleChanged},
    ServerEvent,
};
use uplink_storage::rusqlite::{params, Connection, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Op, Registry};

pub fn register(registry: &mut Registry) {
    registry.register::<AlwaysShowImages>();
}

/// The rule's `image_rule` row: its kind and value.
fn row(rule: &ImageRule) -> (&'static str, &str) {
    match rule {
        ImageRule::Sender { address } => ("sender", address),
        ImageRule::Domain { domain } => ("domain", domain),
        ImageRule::Host { host } => ("host", host),
    }
}

/// The rule, lowercased, if it is one: an address, a domain or a host
/// (as images have them), without spaces.
pub fn normalized(rule: ImageRule) -> Option<ImageRule> {
    let (kind, value) = row(&rule);
    let value = value.trim().to_lowercase();
    if value.is_empty() || value.contains(char::is_whitespace) {
        return None;
    }
    let at = value.split('@').count() - 1;
    Some(match kind {
        "sender" if at == 1 && !value.starts_with('@') && !value.ends_with('@') => {
            ImageRule::Sender { address: value }
        }
        "domain" if at == 0 => ImageRule::Domain { domain: value },
        // As an image's host would be.
        "host"
            if crate::remote::remote(&format!("https://{value}/"))
                .is_some_and(|(host, _)| host == value) =>
        {
            ImageRule::Host { host: value }
        }
        _ => return None,
    })
}

/// The rules the user has of those that would apply to mail from `sender`
/// (as `From:` has it) with images at `hosts`.
pub fn applying<'a>(
    conn: &Connection,
    sender: Option<&str>,
    hosts: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ImageRule>> {
    let sender = sender.map(str::to_lowercase);
    let of_sender = sender.iter().flat_map(|address| {
        let domain = address
            .rsplit_once('@')
            .map(|(_, domain)| domain.to_owned());
        [Some(ImageRule::Sender {
            address: address.clone(),
        })]
        .into_iter()
        .chain([domain.map(|domain| ImageRule::Domain { domain })])
        .flatten()
    });
    let of_hosts = hosts.into_iter().map(|host| ImageRule::Host {
        host: host.to_owned(),
    });
    let mut had = vec![];
    for rule in of_sender.chain(of_hosts) {
        if has(conn, &rule)? {
            had.push(rule);
        }
    }
    Ok(had)
}

fn has(conn: &Connection, rule: &ImageRule) -> Result<bool> {
    let (kind, value) = row(rule);
    Ok(conn
        .query_row(
            "SELECT 1 FROM image_rule WHERE kind = ?1 AND value = ?2",
            params![kind, value],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// What the rule has shown, for descriptions.
fn shown(rule: &ImageRule) -> String {
    match rule {
        ImageRule::Sender { address } => format!("images from {address}"),
        ImageRule::Domain { domain } => format!("images from anyone at {domain}"),
        ImageRule::Host { host } => format!("images hosted at {host}"),
    }
}

/// Has the rule (normalized) show remote images unasked, or no longer.
/// Refused if it already does, or does not: undoing expects to find what
/// was done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlwaysShowImages {
    pub rule: ImageRule,
    pub always: bool,
}

impl Op for AlwaysShowImages {
    const KIND: &'static str = "mail.always_show_images";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let rule = self.rule;
        let (kind, value) = row(&rule);
        let changed = if self.always {
            tx.execute(
                "INSERT INTO image_rule (kind, value) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                params![kind, value],
            )?
        } else {
            tx.execute(
                "DELETE FROM image_rule WHERE kind = ?1 AND value = ?2",
                params![kind, value],
            )?
        };
        if changed == 0 {
            bail!(Conflict(match self.always {
                true => format!("Already always showing {}", shown(&rule)),
                false => format!("Not always showing {}", shown(&rule)),
            }));
        }
        Ok(Applied {
            description: match self.always {
                true => format!("Always showing {}", shown(&rule)),
                false => format!("No longer always showing {}", shown(&rule)),
            },
            inverse: Inverse::of(&AlwaysShowImages {
                rule: rule.clone(),
                always: !self.always,
            }),
            events: vec![ServerEvent::ImageRuleChanged(ImageRuleChanged {
                rule,
                always: self.always,
            })],
        })
    }
}
