//! Senders whose mail's remote images the user has shown unasked (SPEC
//! §8.9), by address, lowercased. Only uplink keeps them: no provider
//! has them to mirror.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uplink_protocol::{mail::ImageSenderChanged, ServerEvent};
use uplink_storage::rusqlite::{Connection, OptionalExtension, Transaction};
use uplink_tx_log::{Applied, Conflict, Inverse, Op, Registry};

pub fn register(registry: &mut Registry) {
    registry.register::<AlwaysShowImages>();
}

/// Whether mail from `sender` (as `From:` has it) has its remote images
/// shown unasked.
pub fn always(conn: &Connection, sender: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM image_sender WHERE address = ?1",
            [sender.to_lowercase()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Shows the remote images of mail from `sender` unasked, or no longer.
/// Refused if they already are, or are not: undoing expects to find what
/// was done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlwaysShowImages {
    pub sender: String,
    pub always: bool,
}

impl Op for AlwaysShowImages {
    const KIND: &'static str = "mail.always_show_images";

    fn apply(self, tx: &Transaction) -> Result<Applied> {
        let sender = self.sender.trim().to_lowercase();
        let changed = if self.always {
            tx.execute(
                "INSERT INTO image_sender (address) VALUES (?1) ON CONFLICT DO NOTHING",
                [&sender],
            )?
        } else {
            tx.execute("DELETE FROM image_sender WHERE address = ?1", [&sender])?
        };
        if changed == 0 {
            bail!(Conflict(match self.always {
                true => format!("Images from {sender} are always shown already"),
                false => format!("Images from {sender} are not always shown"),
            }));
        }
        Ok(Applied {
            description: match self.always {
                true => format!("Always showing images from {sender}"),
                false => format!("No longer always showing images from {sender}"),
            },
            inverse: Inverse::of(&AlwaysShowImages {
                sender: sender.clone(),
                always: !self.always,
            }),
            events: vec![ServerEvent::ImageSenderChanged(ImageSenderChanged {
                sender,
                always: self.always,
            })],
        })
    }
}
