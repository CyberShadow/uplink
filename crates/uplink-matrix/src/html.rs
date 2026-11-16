//! Messages' `formatted_body`, sanitized for the web client to show
//! (SPEC §9.2): the HTML the Matrix spec lets clients show, and no more.
//! Images show only media of the homeserver's, which the server fetches
//! in the client's stead (SPEC §9.8): nothing remote loads.

use ammonia::{Builder, UrlRelative, UrlRelativeEvaluate};
use matrix_sdk::ruma::MxcUri;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use uplink_protocol::matrix::{RoomEventId, RoomId};

/// The tags the Matrix spec suggests clients show
/// (client-server API, "m.room.message msgtypes").
pub(crate) const TAGS: &[&str] = &[
    "font",
    "del",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "p",
    "a",
    "ul",
    "ol",
    "sup",
    "sub",
    "li",
    "b",
    "i",
    "u",
    "strong",
    "em",
    "s",
    "code",
    "hr",
    "br",
    "div",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "caption",
    "pre",
    "span",
    "img",
    "details",
    "summary",
];

/// The content's `formatted_body`, if it is HTML.
fn formatted_body(content: &Value) -> Option<&str> {
    (content["format"] == "org.matrix.custom.html")
        .then(|| content["formatted_body"].as_str())
        .flatten()
}

/// A sanitizer of the HTML the Matrix spec lets clients show: its tags
/// and attributes, but for a reply's fallback, which the client shows on
/// its own.
fn matrix_html() -> Builder<'static> {
    let attributes = HashMap::from([
        (
            "font",
            HashSet::from(["color", "data-mx-bg-color", "data-mx-color"]),
        ),
        (
            "span",
            HashSet::from([
                "data-mx-bg-color",
                "data-mx-color",
                "data-mx-spoiler",
                "data-mx-maths",
            ]),
        ),
        ("a", HashSet::from(["href"])),
        (
            "img",
            HashSet::from(["width", "height", "alt", "title", "src", "data-mx-emoticon"]),
        ),
        ("ol", HashSet::from(["start"])),
        ("code", HashSet::from(["class"])),
        ("div", HashSet::from(["data-mx-maths"])),
    ]);
    let mut builder = Builder::empty();
    builder
        .tags(TAGS.iter().copied().collect())
        .tag_attributes(attributes)
        .generic_attributes(HashSet::new())
        .url_schemes(HashSet::from([
            "http", "https", "ftp", "mailto", "magnet", "mxc",
        ]))
        .clean_content_tags(HashSet::from(["mx-reply", "script", "style"]));
    builder
}

/// Of a code element's `class`, only the language of a code block.
fn code_class(value: &str) -> Option<Cow<'_, str>> {
    (value.starts_with("language-") && !value.contains(' ')).then(|| value.into())
}

/// The content's `formatted_body`, if it is HTML, sanitized; its
/// images, of event `event` (whose content it is, or whose edit's new
/// one), are served by the server.
pub fn sanitized(room: RoomId, event: RoomEventId, content: &Value) -> Option<String> {
    let html = formatted_body(content)?;
    let media = media_prefix(room, event);
    let served = media.clone();
    let clean = matrix_html()
        // Of uplink's own, only the event's media, as the filter serves them.
        .url_relative(UrlRelative::Custom(Box::new(Media(served))))
        .set_tag_attribute_value("a", "target", "_blank")
        .attribute_filter(
            move |element, attribute, value| match (element, attribute) {
                ("img", "src") => media_path(value).map(|path| format!("{media}{path}").into()),
                (_, _) if value.starts_with("mxc:") => None,
                ("code", "class") => code_class(value),
                _ => Some(value.into()),
            },
        )
        .clean(html)
        .to_string();
    Some(clean)
}

/// The content's `formatted_body`, if it is HTML, sanitized as an edit
/// of it would send it again: its media as the homeserver's (`mxc:`),
/// and nothing relative, which is of no server.
pub fn as_sent(content: &Value) -> Option<String> {
    let html = formatted_body(content)?;
    let clean = matrix_html()
        .url_relative(UrlRelative::Deny)
        .link_rel(None)
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("code", "class") => code_class(value),
            _ => Some(value.into()),
        })
        .clean(html)
        .to_string();
    Some(clean)
}

/// Where the server serves the media of `event` that its HTML has: its
/// `server/id` after.
fn media_prefix(room: RoomId, event: RoomEventId) -> String {
    format!("/api/rooms/{}/events/{}/media/", room.0, event.0)
}

/// The `server/id` of media `mxc`, if it is a well-formed `mxc:` URI, as
/// a path that a browser takes as it is. Ruma takes `.` and `..` for
/// server names (of DNS's characters); in a path, a browser resolves
/// them, to anything of uplink's.
fn media_path(mxc: &str) -> Option<&str> {
    let (server, _) = <&MxcUri>::from(mxc).parts().ok()?;
    (!matches!(server.as_str(), "." | "..")).then(|| &mxc["mxc://".len()..])
}

/// Keeps relative URLs of the event's media only (under its prefix),
/// well-formed, as the filter serves them.
struct Media(String);

impl<'a> UrlRelativeEvaluate<'a> for Media {
    fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
        let path = url.strip_prefix(&self.0)?;
        (media_path(&format!("mxc://{path}")) == Some(path)).then_some(Cow::Borrowed(url))
    }
}

#[cfg(test)]
mod hostile;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clean(html: &str) -> String {
        let content = json!({ "format": "org.matrix.custom.html", "formatted_body": html });
        sanitized(RoomId(3), RoomEventId(7), &content).unwrap()
    }

    #[test]
    fn only_what_matrix_lets_through_is_shown() {
        assert_eq!(
            clean(
                r#"<b onclick="x()">hi</b><script>x()</script><style>*{}</style><iframe></iframe>"#
            ),
            "<b>hi</b>"
        );
        assert_eq!(
            clean(
                r#"<a href="https://x.org" style="color:red">x</a><a href="javascript:x()">y</a>"#
            ),
            r#"<a href="https://x.org" target="_blank" rel="noopener noreferrer">x</a><a target="_blank" rel="noopener noreferrer">y</a>"#
        );
        assert_eq!(
            clean(r#"<a href="/api/blobs">x</a>"#),
            r#"<a rel="noopener noreferrer" target="_blank">x</a>"#,
            "nothing of uplink's own"
        );
        assert_eq!(
            clean(
                r#"<pre><code class="language-rust x">a</code><code class="language-rust">b</code></pre>"#
            ),
            r#"<pre><code>a</code><code class="language-rust">b</code></pre>"#
        );
        assert_eq!(
            clean(r##"<span data-mx-spoiler="why" data-mx-color="#ff0000" class="x">s</span>"##),
            r##"<span data-mx-spoiler="why" data-mx-color="#ff0000">s</span>"##
        );
    }

    #[test]
    fn replies_fallbacks_are_dropped() {
        assert_eq!(
            clean("<mx-reply><blockquote>quoted <b>it</b></blockquote></mx-reply>answer"),
            "answer"
        );
    }

    #[test]
    fn images_are_the_homeservers_only() {
        assert_eq!(
            clean(
                r#"<img src="mxc://x.org/cat" alt=":cat:" data-mx-emoticon><img src="https://x.org/t.png">"#
            ),
            r#"<img src="/api/rooms/3/events/7/media/x.org/cat" alt=":cat:" data-mx-emoticon=""><img>"#
        );
        assert_eq!(
            clean(r#"<a href="mxc://x.org/cat">x</a>"#),
            r#"<a rel="noopener noreferrer" target="_blank">x</a>"#
        );
        // Not those that, resolved, would be anything else of uplink's.
        assert_eq!(
            clean(
                r#"<img src="mxc://../cat"><img src="mxc://x.org/../../cat"><img src="mxc://x.org/%2e%2e">"#
            ),
            "<img><img><img>"
        );
        // A port, an IPv6 address.
        assert_eq!(
            clean(r#"<img src="mxc://x.org:8448/cat"><img src="mxc://[::1]/cat">"#),
            r#"<img src="/api/rooms/3/events/7/media/x.org:8448/cat"><img src="/api/rooms/3/events/7/media/[::1]/cat">"#
        );
    }

    #[test]
    fn plain_bodies_are_not_html() {
        let content = json!({ "body": "<b>not bold</b>" });
        assert_eq!(sanitized(RoomId(3), RoomEventId(7), &content), None);
        assert_eq!(as_sent(&content), None);
    }

    #[test]
    fn as_sent_keeps_the_homeservers_media_and_adds_nothing() {
        let content = json!({
            "format": "org.matrix.custom.html",
            "formatted_body": concat!(
                r#"<mx-reply><blockquote>old</blockquote></mx-reply>"#,
                r#"<a href="https://x.org" style="color:red">x</a><a href="/api/blobs">y</a>"#,
                r#"<img src="mxc://x.org/cat" alt=":cat:" data-mx-emoticon><b onclick="x()">b</b>"#,
                r#"<pre><code class="language-rust x">a</code><code class="language-rust">b</code></pre>"#,
            ),
        });
        assert_eq!(
            as_sent(&content).unwrap(),
            concat!(
                r#"<a href="https://x.org">x</a><a>y</a>"#,
                r#"<img src="mxc://x.org/cat" alt=":cat:" data-mx-emoticon=""><b>b</b>"#,
                r#"<pre><code>a</code><code class="language-rust">b</code></pre>"#,
            )
        );
    }
}
