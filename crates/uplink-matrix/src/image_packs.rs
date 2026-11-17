//! Image packs (MSC2545, SPEC §9.9): custom emoji, as rooms' state
//! (`im.ponies.room_emotes`, a pack by state key), the account's own
//! (`im.ponies.user_emotes`), and rooms' packs it uses everywhere
//! (`im.ponies.emote_rooms`), as a bridge's are (a Slack workspace's, in
//! its space). Of each, only images usable as emoji are stored.
//!
//! A room offers the packs it may react with, as Cinny's: the account's
//! own, the room's, those of the spaces it is in (and theirs, up), and
//! those used everywhere; of joined rooms only.

use matrix_sdk::ruma::MxcUri;
use serde::Serialize;
use serde_json::Value;
use uplink_protocol::matrix::{ImagePack, PackImage, RoomId};
use uplink_storage::rusqlite::{params, OptionalExtension, Transaction};

use anyhow::Result;

/// A room's packs, by state key.
pub const ROOM_PACKS: &str = "im.ponies.room_emotes";
/// The account's own pack, of its account data.
pub const USER_PACK: &str = "im.ponies.user_emotes";
/// The rooms' packs the account uses everywhere, of its account data.
pub const EMOTE_ROOMS: &str = "im.ponies.emote_rooms";

/// A pack, as its content says: its name, and its images usable as
/// emoji, by shortcode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pack {
    /// `pack.display_name`, if a string.
    pub name: Option<String>,
    pub images: Vec<PackImage>,
}

impl Pack {
    /// Reads a pack's content. Its images without an `mxc:` URI, or
    /// whose shortcode is empty or has a colon or white space (it is
    /// written between colons), are left out; as are those it says are
    /// not emoji (`usage`, the image's, else the pack's, without
    /// `emoticon`; without either, an image is both emoji and sticker).
    pub fn parse(content: &Value) -> Self {
        let emoticon = |usage: &Value| -> Option<bool> {
            let usage = usage.as_array().filter(|u| !u.is_empty())?;
            Some(usage.iter().any(|u| u == "emoticon"))
        };
        let of_pack = emoticon(&content["pack"]["usage"]);
        let images = content["images"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(shortcode, image)| {
                let url = image["url"].as_str()?;
                let valid = !shortcode.is_empty()
                    && !shortcode.contains(|c: char| c == ':' || c.is_whitespace())
                    && <&MxcUri>::from(url).is_valid();
                let usable = emoticon(&image["usage"]).or(of_pack).unwrap_or(true);
                (valid && usable).then(|| PackImage {
                    shortcode: shortcode.clone(),
                    url: url.to_owned(),
                    body: image["body"].as_str().map(str::to_owned),
                })
            })
            .collect();
        Self {
            name: content["pack"]["display_name"].as_str().map(str::to_owned),
            images,
        }
    }
}

/// Reads `im.ponies.emote_rooms`: the packs used everywhere, by room id
/// and state key.
pub fn parse_emote_rooms(content: &Value) -> Vec<(String, String)> {
    let mut packs: Vec<(String, String)> = content["rooms"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(room, keys)| {
            keys.as_object()
                .into_iter()
                .flatten()
                .map(move |(key, _)| (room.clone(), key.clone()))
        })
        .collect();
    packs.sort();
    packs
}

/// What the account's account data says of packs.
#[derive(Debug, Serialize)]
pub struct AccountPacks {
    /// Its own pack, if it has one.
    pub user: Option<Pack>,
    /// The rooms' packs it uses everywhere, by room id and state key.
    pub everywhere: Vec<(String, String)>,
}

/// Records the room's packs, by state key, in its order, if they changed.
pub fn observe_room(
    tx: &Transaction,
    account: &str,
    room: RoomId,
    packs: &[(String, Pack)],
) -> Result<()> {
    if stored(tx, account, Some(room))? != packs {
        tx.execute("DELETE FROM matrix_image_pack WHERE room = ?1", [room.0])?;
        for (key, pack) in packs {
            insert(tx, account, Some(room), key, pack)?;
        }
    }
    Ok(())
}

/// Records what the account's account data says of packs.
pub fn observe_account(tx: &Transaction, account: &str, packs: &AccountPacks) -> Result<()> {
    let user: Vec<(String, Pack)> = packs
        .user
        .iter()
        .map(|p| (String::new(), p.clone()))
        .collect();
    if stored(tx, account, None)? != user {
        tx.execute(
            "DELETE FROM matrix_image_pack WHERE account_id = ?1 AND room IS NULL",
            [account],
        )?;
        for (key, pack) in &user {
            insert(tx, account, None, key, pack)?;
        }
    }
    tx.execute(
        "DELETE FROM matrix_emote_room WHERE account_id = ?1",
        [account],
    )?;
    let mut everywhere = tx.prepare(
        "INSERT INTO matrix_emote_room (account_id, room_id, state_key) VALUES (?1, ?2, ?3)",
    )?;
    for (room, key) in &packs.everywhere {
        everywhere.execute(params![account, room, key])?;
    }
    Ok(())
}

/// The packs stored of `room` (`None`: the account's own), by state key.
fn stored(tx: &Transaction, account: &str, room: Option<RoomId>) -> Result<Vec<(String, Pack)>> {
    let packs: Vec<(i64, String, Option<String>)> = tx
        .prepare(
            "SELECT id, state_key, name FROM matrix_image_pack
             WHERE account_id = ?1 AND room IS ?2 ORDER BY state_key",
        )?
        .query_map(params![account, room.map(|r| r.0)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    packs
        .into_iter()
        .map(|(id, key, name)| {
            let images = images(tx, id)?;
            Ok((key, Pack { name, images }))
        })
        .collect()
}

/// The pack's images, by shortcode.
fn images(tx: &Transaction, pack: i64) -> Result<Vec<PackImage>> {
    Ok(tx
        .prepare_cached(
            "SELECT shortcode, url, body FROM matrix_pack_image
             WHERE pack = ?1 ORDER BY shortcode",
        )?
        .query_map([pack], |r| {
            Ok(PackImage {
                shortcode: r.get(0)?,
                url: r.get(1)?,
                body: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

fn insert(
    tx: &Transaction,
    account: &str,
    room: Option<RoomId>,
    key: &str,
    pack: &Pack,
) -> Result<()> {
    let id: i64 = tx.query_row(
        "INSERT INTO matrix_image_pack (account_id, room, state_key, name)
         VALUES (?1, ?2, ?3, ?4) RETURNING id",
        params![account, room.map(|r| r.0), key, pack.name],
        |r| r.get(0),
    )?;
    let mut image = tx.prepare(
        "INSERT INTO matrix_pack_image (pack, shortcode, url, body) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for i in &pack.images {
        image.execute(params![id, i.shortcode, i.url, i.body])?;
    }
    Ok(())
}

/// In SQL, `offered(pack, rank)`: the packs room `?1` offers, ranked: the
/// account's own, the room's, its spaces', those used everywhere.
const OFFERED: &str = "
WITH RECURSIVE here(id, account_id) AS (
  SELECT id, account_id FROM matrix_room WHERE id = ?1
),
up(id) AS (
  SELECT id FROM here
  UNION
  SELECT s.space FROM up
  JOIN matrix_room r ON r.id = up.id
  JOIN matrix_space_child s ON s.child = r.room_id
  JOIN matrix_room space ON space.id = s.space AND space.account_id = r.account_id
),
candidate(pack, rank) AS (
  SELECT p.id, 0 FROM matrix_image_pack p JOIN here ON p.account_id = here.account_id
  WHERE p.room IS NULL
  UNION ALL
  SELECT p.id, CASE WHEN p.room = ?1 THEN 1 ELSE 2 END
  FROM matrix_image_pack p JOIN up ON p.room = up.id
  UNION ALL
  SELECT p.id, 3 FROM matrix_image_pack p
  JOIN matrix_room r ON r.id = p.room
  JOIN here ON r.account_id = here.account_id
  JOIN matrix_emote_room g
    ON g.account_id = r.account_id AND g.room_id = r.room_id AND g.state_key = p.state_key
),
offered(pack, rank) AS (
  SELECT c.pack, min(c.rank) FROM candidate c
  JOIN matrix_image_pack p ON p.id = c.pack
  LEFT JOIN matrix_room r ON r.id = p.room
  WHERE p.room IS NULL OR r.membership = 'joined'
  GROUP BY c.pack
)";

/// The packs the room offers, in order, with their images; `None` if
/// there is no such room. A pack without a name is named after its room,
/// but the account's own.
pub fn offered(tx: &Transaction, room: RoomId) -> Result<Option<Vec<ImagePack>>> {
    let exists: Option<i64> = tx
        .query_row("SELECT 1 FROM matrix_room WHERE id = ?1", [room.0], |r| {
            r.get(0)
        })
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let packs: Vec<(i64, Option<String>)> = tx
        .prepare(&format!(
            "{OFFERED}
             SELECT p.id, coalesce(p.name, r.name) FROM offered o
             JOIN matrix_image_pack p ON p.id = o.pack
             LEFT JOIN matrix_room r ON r.id = p.room
             ORDER BY o.rank, r.name, p.state_key"
        ))?
        .query_map([room.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let packs = packs
        .into_iter()
        .map(|(id, name)| {
            let images = images(tx, id)?;
            Ok(ImagePack { name, images })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(packs))
}

/// The account of the room, if one of the packs it offers has the image
/// `mxc`.
pub fn offering(tx: &Transaction, room: RoomId, mxc: &str) -> Result<Option<String>> {
    Ok(tx
        .query_row(
            &format!(
                "{OFFERED}
                 SELECT p.account_id FROM offered o
                 JOIN matrix_image_pack p ON p.id = o.pack
                 JOIN matrix_pack_image i ON i.pack = p.id
                 WHERE i.url = ?2 LIMIT 1"
            ),
            params![room.0, mxc],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shortcodes(pack: &Pack) -> Vec<&str> {
        pack.images.iter().map(|i| i.shortcode.as_str()).collect()
    }

    #[test]
    fn only_valid_images_usable_as_emoji_are_read() {
        let pack = Pack::parse(&json!({
            "pack": { "display_name": "Work", "usage": ["emoticon"] },
            "images": {
                "party": { "url": "mxc://x/party", "body": "party parrot" },
                "sticker": { "url": "mxc://x/sticker", "usage": ["sticker"] },
                "both": { "url": "mxc://x/both", "usage": ["sticker", "emoticon"] },
                "a:b": { "url": "mxc://x/colon" },
                "a b": { "url": "mxc://x/space" },
                "nourl": {},
                "http": { "url": "https://x/http" },
            },
        }));
        assert_eq!(pack.name.as_deref(), Some("Work"));
        assert_eq!(shortcodes(&pack), ["both", "party"]);
        assert_eq!(pack.images[1].body.as_deref(), Some("party parrot"));

        let stickers = Pack::parse(&json!({
            "pack": { "usage": ["sticker"] },
            "images": {
                "s": { "url": "mxc://x/s" },
                "e": { "url": "mxc://x/e", "usage": ["emoticon"] },
            },
        }));
        assert_eq!(shortcodes(&stickers), ["e"], "the image's usage first");
        let plain = Pack::parse(&json!({ "images": { "p": { "url": "mxc://x/p" } } }));
        assert_eq!(
            (plain.name.as_deref(), shortcodes(&plain).len()),
            (None, 1),
            "without usage, both"
        );
        assert!(Pack::parse(&json!({})).images.is_empty());
    }

    #[test]
    fn packs_used_everywhere_are_read_by_room_and_state_key() {
        let rooms = parse_emote_rooms(&json!({ "rooms": {
            "!b:x": { "": {} },
            "!a:x": { "slack": {}, "other": {} },
            "!bad:x": "nope",
        } }));
        assert_eq!(
            rooms,
            [
                ("!a:x".to_owned(), "other".to_owned()),
                ("!a:x".to_owned(), "slack".to_owned()),
                ("!b:x".to_owned(), String::new()),
            ]
        );
        assert!(parse_emote_rooms(&json!({})).is_empty());
    }
}
