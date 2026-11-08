//! What uplink takes from a raw message: identity, listing headers and the
//! text to index (SPEC §4.4–4.5).

use base64::Engine;
use mail_parser::{
    Address, HeaderName, Message, MessageParser, MessagePart, MimeHeaders, PartType,
};
use std::collections::HashMap;
use uplink_protocol::mail::Attachment;

/// Characters of body text shown in message lists.
const SNIPPET_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// Without angle brackets.
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    /// The `Date:` header, unix seconds.
    pub date_sent: Option<i64>,
    /// Lowercased addresses this copy may name as its sender, sorted: the
    /// `From:` addresses, plus, for mail through a mailing list, the
    /// original sender's (`X-Original-From:`, `Reply-To:`), since lists
    /// rewrite `From:` for DMARC.
    pub senders: Vec<String>,
    /// Plain text of the first text part, or of the first HTML part
    /// converted.
    pub body_text: String,
    pub snippet: String,
}

pub fn parse(raw: &[u8]) -> Parsed {
    // mail-parser's built-in header parsers apply only while no custom one
    // is registered, so name every kind of header used.
    let parser = MessageParser::new()
        .with_mime_headers()
        .with_date_headers()
        .with_address_headers()
        .with_message_ids()
        .header_text(HeaderName::Subject)
        .header_address(HeaderName::Other("X-Original-From".into()));
    // mail-parser accepts anything; an unparseable message has no headers
    // and no body, and is still stored and listed.
    let Some(message) = parser.parse(raw) else {
        return Parsed {
            message_id: None,
            subject: None,
            from_addr: None,
            from_name: None,
            to_addrs: vec![],
            cc_addrs: vec![],
            date_sent: None,
            senders: vec![],
            body_text: String::new(),
            snippet: String::new(),
        };
    };

    let from = message.from().and_then(Address::first);
    let mut senders: Vec<String> = addresses(message.from());
    if message.header(HeaderName::ListId).is_some() {
        let original_from = message
            .header("X-Original-From")
            .and_then(|value| value.as_address());
        senders.extend(addresses(original_from));
        senders.extend(addresses(message.reply_to()));
    }
    let mut senders: Vec<String> = senders.iter().map(|a| a.to_lowercase()).collect();
    senders.sort();
    senders.dedup();

    let body_text = message
        .body_text(0)
        .map(|text| text.into_owned())
        .unwrap_or_default();

    Parsed {
        message_id: message.message_id().map(str::to_owned),
        subject: message.subject().map(str::to_owned),
        from_addr: from.and_then(|a| a.address()).map(str::to_owned),
        from_name: from.and_then(|a| a.name()).map(str::to_owned),
        to_addrs: addresses(message.to()),
        cc_addrs: addresses(message.cc()),
        date_sent: message.date().map(|date| date.to_timestamp()),
        senders,
        snippet: snippet(&body_text),
        body_text,
    }
}

/// The message's HTML body, if it has one, sanitized for display: no
/// scripts, event handlers or forms; styles and presentational attributes
/// stay, since mail layout depends on them. Remote resources are left in
/// place for the client to block or allow (SPEC §8.9). Images of the
/// message's own (`cid:`) are inlined as `data:` URIs.
pub fn html_body(raw: &[u8]) -> Option<String> {
    let message = MessageParser::new().parse(raw)?;
    let PartType::Html(html) = &message.html_part(0)?.body else {
        // A text-only message: its text part stands in as the HTML one.
        return None;
    };
    let images: HashMap<String, String> = message
        .attachments()
        .filter_map(|part| {
            let cid = part.content_id()?;
            let data = base64::engine::general_purpose::STANDARD.encode(part.contents());
            Some((cid.to_owned(), format!("data:{};base64,{data}", mime(part))))
        })
        .collect();
    Some(
        ammonia::Builder::default()
            // Let `cid:` through the scheme check, for the filter below.
            .add_url_schemes(["cid"])
            .attribute_filter(move |element, attribute, value| {
                let Some(cid) = value.strip_prefix("cid:") else {
                    return Some(value.into());
                };
                match (element, attribute) {
                    ("img", "src") => images.get(cid).map(|uri| uri.clone().into()),
                    _ => None,
                }
            })
            .rm_clean_content_tags(["style"])
            .add_tags(["style", "font", "center"])
            .add_generic_attributes([
                "style",
                "class",
                "id",
                "width",
                "height",
                "align",
                "valign",
                "bgcolor",
                "color",
                "border",
                "cellpadding",
                "cellspacing",
                "face",
                "size",
                "dir",
            ])
            .clean(html)
            .to_string(),
    )
}

/// The message's attachments, inline images included: those its HTML body
/// shows are marked `inline`. An attachment's index is its place among
/// them, as [`attachment`] takes it.
pub fn attachments(raw: &[u8]) -> Vec<Attachment> {
    let Some(message) = MessageParser::new().parse(raw) else {
        return vec![];
    };
    let html = html_source(&message);
    message
        .attachments()
        .enumerate()
        .map(|(index, part)| Attachment {
            index: index.try_into().expect("fewer than 2^32 attachments"),
            name: part.attachment_name().map(str::to_owned),
            mime: mime(part),
            size: part.contents().len().try_into().expect("size fits in u64"),
            inline: part
                .content_id()
                .is_some_and(|cid| html.is_some_and(|html| html.contains(&format!("cid:{cid}")))),
        })
        .collect()
}

/// The attachment at `index`: its name, MIME type and decoded content.
pub fn attachment(raw: &[u8], index: u32) -> Option<(Option<String>, String, Vec<u8>)> {
    let message = MessageParser::new().parse(raw)?;
    let part = message.attachments().nth(index as usize)?;
    Some((
        part.attachment_name().map(str::to_owned),
        mime(part),
        part.contents().to_vec(),
    ))
}

fn html_source<'a>(message: &'a Message) -> Option<&'a str> {
    match &message.html_part(0)?.body {
        PartType::Html(html) => Some(html),
        _ => None,
    }
}

/// `type/subtype`, lowercased; mail's default if the part names none.
fn mime(part: &MessagePart) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_lowercase(),
            None => ct.ctype().to_lowercase(),
        },
        None => "application/octet-stream".to_owned(),
    }
}

fn addresses(address: Option<&Address>) -> Vec<String> {
    address
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.address())
        .map(str::to_owned)
        .collect()
}

fn snippet(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    words.join(" ").chars().take(SNIPPET_CHARS).collect()
}

/// The longest prefix of `text` of at most `max_bytes`.
pub fn truncate(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(headers: &str, body: &str) -> Vec<u8> {
        format!("{}\r\n\r\n{body}", headers.replace('\n', "\r\n")).into_bytes()
    }

    #[test]
    fn headers_and_body() {
        let p = parse(&raw(
            "Message-ID: <abc@example.com>\n\
             From: Alice Example <Alice@Example.com>\n\
             To: bob@example.com, Carol <carol@example.com>\n\
             Cc: dave@example.com\n\
             Subject: Lunch?\n\
             Date: Tue, 3 Nov 2026 10:00:00 +0100",
            "Shall we   meet\nat noon?\n",
        ));
        assert_eq!(p.message_id.as_deref(), Some("abc@example.com"));
        assert_eq!(p.subject.as_deref(), Some("Lunch?"));
        assert_eq!(p.from_addr.as_deref(), Some("Alice@Example.com"));
        assert_eq!(p.from_name.as_deref(), Some("Alice Example"));
        assert_eq!(p.to_addrs, ["bob@example.com", "carol@example.com"]);
        assert_eq!(p.cc_addrs, ["dave@example.com"]);
        assert_eq!(p.date_sent, Some(1_793_696_400));
        assert_eq!(p.senders, ["alice@example.com"]);
        assert_eq!(p.snippet, "Shall we meet at noon?");
    }

    #[test]
    fn html_only_body_is_converted() {
        let p = parse(&raw(
            "From: a@example.com\nContent-Type: text/html",
            "<p>Hello <b>there</b></p>",
        ));
        assert_eq!(p.snippet, "Hello there");
    }

    #[test]
    fn list_mail_names_the_original_sender() {
        let p = parse(&raw(
            "From: 'Alice' via Group <group@lists.example.org>\n\
             X-Original-From: Alice <alice@example.com>\n\
             Reply-To: Alice <alice@example.com>, group@lists.example.org\n\
             List-Id: <group.lists.example.org>",
            "hi",
        ));
        assert_eq!(p.senders, ["alice@example.com", "group@lists.example.org"]);
    }

    #[test]
    fn reply_to_is_ignored_outside_lists() {
        let p = parse(&raw("From: a@example.com\nReply-To: b@example.com", "hi"));
        assert_eq!(p.senders, ["a@example.com"]);
    }

    #[test]
    fn html_bodies_are_sanitized() {
        let html = html_body(&raw(
            "From: a@example.com\nContent-Type: text/html",
            "<style>p { color: red }</style><p style=\"margin: 0\" onclick=\"x()\">Hi\
             <script>alert(1)</script><img src=\"https://t.example/p.gif\"></p>",
        ))
        .unwrap();
        assert!(html.contains("<style>p { color: red }</style>"), "{html}");
        assert!(html.contains("style=\"margin: 0\""), "{html}");
        assert!(html.contains("https://t.example/p.gif"), "{html}");
        assert!(
            !html.contains("onclick") && !html.contains("script"),
            "{html}"
        );
        assert_eq!(html_body(&raw("From: a@example.com", "plain")), None);
    }

    /// An HTML message showing an image of its own, with a PDF attached.
    const WITH_PARTS: &str = "From: a@example.com\r
Subject: Report\r
MIME-Version: 1.0\r
Content-Type: multipart/mixed; boundary=outer\r
\r
--outer\r
Content-Type: multipart/related; boundary=inner\r
\r
--inner\r
Content-Type: text/html\r
\r
<p>See <img src=\"cid:logo@x\"> and <a href=\"cid:logo@x\">this</a></p>\r
--inner\r
Content-Type: image/png\r
Content-ID: <logo@x>\r
Content-Transfer-Encoding: base64\r
\r
iVBORw0K\r
--inner--\r
--outer\r
Content-Type: application/PDF; name=\"report.pdf\"\r
Content-Disposition: attachment; filename=\"report.pdf\"\r
Content-Transfer-Encoding: base64\r
\r
JVBERi0xLjQ=\r
--outer--\r
";

    #[test]
    fn attachments_are_listed_and_inline_images_inlined() {
        let raw = WITH_PARTS.as_bytes();
        let listed = attachments(raw);
        assert_eq!(
            listed,
            [
                Attachment {
                    index: 0,
                    name: None,
                    mime: "image/png".into(),
                    size: 6,
                    inline: true,
                },
                Attachment {
                    index: 1,
                    name: Some("report.pdf".into()),
                    mime: "application/pdf".into(),
                    size: 8,
                    inline: false,
                },
            ]
        );
        assert_eq!(
            attachment(raw, 1),
            Some((
                Some("report.pdf".into()),
                "application/pdf".into(),
                b"%PDF-1.4".to_vec()
            ))
        );
        assert_eq!(attachment(raw, 2), None);
        let html = html_body(raw).unwrap();
        assert!(
            html.contains("<img src=\"data:image/png;base64,iVBORw0K\">"),
            "{html}"
        );
        // Only images: a link to a part goes nowhere.
        assert!(!html.contains("cid:"), "{html}");
    }

    #[test]
    fn missing_headers() {
        let p = parse(b"\r\njust a body");
        assert_eq!(p.message_id, None);
        assert_eq!(p.date_sent, None);
        assert!(p.senders.is_empty());
    }

    #[test]
    fn body_is_truncated_on_a_char_boundary() {
        assert_eq!(truncate("aé", 2), "a");
        assert_eq!(truncate("aé", 3), "aé");
    }
}
