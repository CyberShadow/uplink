//! The media of events (SPEC §9.8): images, videos, audio and files sent
//! to rooms, their thumbnails, and images in formatted messages, fetched
//! by the account's client (decrypting those of encrypted rooms) for the
//! web client to show.
//!
//! Only media an event names are fetched, so that the web client cannot
//! have the server fetch whatever it names.

use matrix_sdk::ruma::events::room::{EncryptedFile, MediaSource};
use matrix_sdk::ruma::OwnedMxcUri;
use serde_json::Value;

/// Media an event names, and what it says of them.
#[derive(Debug)]
pub struct Named {
    pub source: MediaSource,
    /// The media type the event gives.
    pub mime: Option<String>,
    /// The file name the event gives.
    pub name: Option<String>,
}

/// The media `mxc` as one of `contents` (an event's, then its edits' new
/// ones) names it: as the file sent (`url`, or encrypted `file`), its
/// thumbnail (`info.thumbnail_url` or `thumbnail_file`), or an image of
/// its `formatted_body`.
pub fn named(contents: &[Value], mxc: &str) -> Option<Named> {
    contents.iter().find_map(|content| in_content(content, mxc))
}

fn in_content(content: &Value, mxc: &str) -> Option<Named> {
    let info = &content["info"];
    let text = |value: &Value| value.as_str().map(str::to_owned);
    // A file's own name: `filename` if it has a caption in `body`.
    let name = text(&content["filename"]).or_else(|| text(&content["body"]));
    let plain = |mime: &Value, name: Option<String>| Named {
        source: MediaSource::Plain(OwnedMxcUri::from(mxc)),
        mime: text(mime),
        name,
    };
    let encrypted = |file: &Value, mime: &Value, name: Option<String>| {
        let file: EncryptedFile = serde_json::from_value(file.clone()).ok()?;
        Some(Named {
            source: MediaSource::Encrypted(Box::new(file)),
            mime: text(mime),
            name,
        })
    };
    if content["url"] == mxc {
        return Some(plain(&info["mimetype"], name));
    }
    if content["file"]["url"] == mxc {
        return encrypted(&content["file"], &info["mimetype"], name);
    }
    let thumbnail_mime = &info["thumbnail_info"]["mimetype"];
    if info["thumbnail_url"] == mxc {
        return Some(plain(thumbnail_mime, None));
    }
    if info["thumbnail_file"]["url"] == mxc {
        return encrypted(&info["thumbnail_file"], thumbnail_mime, None);
    }
    let html = content["formatted_body"].as_str()?;
    (html.contains(&format!("\"{mxc}\"")) || html.contains(&format!("'{mxc}'")))
        .then(|| plain(&Value::Null, None))
}

/// Media types the browser is let show in uplink's origin: images it
/// decodes (not SVG, which is a document), video and audio. Others are
/// only downloaded.
pub fn shown_inline(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
    ) || mime.starts_with("video/")
        || mime.starts_with("audio/")
}

/// The media type of the image formats homeservers make thumbnails in,
/// by their signatures.
pub fn image_type(image: &[u8]) -> Option<&'static str> {
    match image {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn media_are_found_where_events_name_them() {
        let image = json!({
            "msgtype": "m.image", "body": "cat.png", "url": "mxc://x/cat",
            "info": { "mimetype": "image/png", "thumbnail_url": "mxc://x/thumb",
                      "thumbnail_info": { "mimetype": "image/jpeg" } },
        });
        let found = named(std::slice::from_ref(&image), "mxc://x/cat").unwrap();
        assert!(matches!(found.source, MediaSource::Plain(_)));
        assert_eq!(
            (found.mime.as_deref(), found.name.as_deref()),
            (Some("image/png"), Some("cat.png"))
        );
        let thumb = named(std::slice::from_ref(&image), "mxc://x/thumb").unwrap();
        assert_eq!(
            (thumb.mime.as_deref(), thumb.name),
            (Some("image/jpeg"), None)
        );
        assert!(named(&[image], "mxc://x/other").is_none());

        let file = json!({
            "msgtype": "m.file", "body": "see attached", "filename": "a.pdf",
            "file": { "url": "mxc://x/enc", "v": "v2",
                      "key": { "kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR",
                               "k": "qcHVMSgYg-71CauWBezXI5qkaRb0LuIy-Wx5kIaHMIA", "ext": true },
                      "iv": "X85+XgHN+HEAAAAAAAAAAA",
                      "hashes": { "sha256": "5qG4fFnbbVdlAB1Q72JDKwCagV6Dbkx9uDuTNj6hHfI" } },
            "info": { "mimetype": "application/pdf" },
        });
        let found = named(&[json!({}), file], "mxc://x/enc").unwrap();
        assert!(matches!(found.source, MediaSource::Encrypted(_)));
        assert_eq!(found.name.as_deref(), Some("a.pdf"));

        let html = json!({ "body": ":cat:", "format": "org.matrix.custom.html",
                           "formatted_body": "<img src=\"mxc://x/emoji\" alt=\":cat:\">" });
        assert!(named(std::slice::from_ref(&html), "mxc://x/emoji").is_some());
        assert!(named(&[html], "mxc://x/emo").is_none(), "whole URIs only");
    }

    #[test]
    fn only_media_browsers_play_safely_are_shown() {
        assert!(shown_inline("image/png"));
        assert!(shown_inline("video/mp4"));
        assert!(!shown_inline("image/svg+xml"));
        assert!(!shown_inline("text/html"));
    }
}
