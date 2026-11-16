//! Markdown, of HTML: the text an edit of one's own message begins from
//! (SPEC §12.11), and the plain body of a message sent as HTML (`/html`).
//!
//! A message's `body` is no source to edit: it is a fallback for clients
//! that show no HTML, which a bridge writes as it likes, unescaped (a
//! Slack bridge's `~~struck ~~rest` formats as nothing). An edit begins
//! from its HTML instead, as Markdown that formats as that HTML does, as
//! [`htmd`]'s faithful translation means to: else, it sends that HTML as
//! it is (`/html`).

use crate::html;
use htmd::element_handler::Handlers;
use htmd::options::{BulletListMarker, LinkStyle, Options, TranslationMode};
use htmd::{Element, HtmlToMarkdown, HtmlToMarkdownBuilder};
use html5ever::{local_name, ns, tendril::TendrilSink, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use matrix_sdk::ruma::events::room::message::FormattedBody;
use serde_json::Value;

/// The message types of text, which an edit can be of.
const TEXT: &[&str] = &["m.text", "m.notice", "m.emote"];

/// Of the content of a message of text (the message's own, or its
/// edit's new one), the text an edit of it begins from: what the composer
/// would send as it is. Its HTML as Markdown, if that formats as it does,
/// else `/html` and the HTML; without HTML, its body, as plain text
/// (`/plain`) if Markdown would format it. One that begins with a `/`
/// begins with another, which sends one.
pub fn edit_text(content: &Value) -> Option<String> {
    if !TEXT.contains(&content["msgtype"].as_str()?) {
        return None;
    }
    let body = content["body"].as_str()?;
    Some(match html::as_sent(content) {
        Some(html) => {
            let markdown = faithful(&html);
            match shows_as(&formatted(&markdown), &html) {
                true => unslashed(markdown),
                false => format!("/html {html}"),
            }
        }
        None => match FormattedBody::markdown(body) {
            None => unslashed(body.to_owned()),
            Some(_) => format!("/plain {body}"),
        },
    })
}

/// A message sent as HTML, as plain text: Markdown, of what Markdown
/// spells (and struck text, as GFM strikes it), the rest as text.
pub fn plain(html: &str) -> String {
    builder(TranslationMode::Pure)
        .add_handler(
            vec!["del", "s", "strike"],
            |handlers: &dyn Handlers, element: &Element| {
                let text = handlers.walk_children_content(element.node, element.context);
                let (start, end) = (text.len() - text.trim_start().len(), text.trim_end().len());
                let (before, struck, after) = (&text[..start], &text[start..end], &text[end..]);
                Some(format!("{before}~~{struck}~~{after}").into())
            },
        )
        .build()
        .convert(html)
        .expect("converting HTML to Markdown writes to memory")
}

/// HTML as Markdown that formats as it does, as far as `htmd` can.
fn faithful(html: &str) -> String {
    builder(TranslationMode::Faithful)
        .build()
        .convert(html)
        .expect("converting HTML to Markdown writes to memory")
}

/// A converter of HTML to Markdown, written as one would type it.
fn builder(translation_mode: TranslationMode) -> HtmlToMarkdownBuilder {
    HtmlToMarkdown::builder().options(Options {
        bullet_list_marker: BulletListMarker::Dash,
        ul_bullet_spacing: 1,
        ol_number_spacing: 1,
        link_style: LinkStyle::InlinedPreferAutolinks,
        preformatted_code: true,
        translation_mode,
        ..Options::default()
    })
}

/// What a message of Markdown shows, as HTML: its formatting, or else,
/// formatting nothing, its text.
fn formatted(markdown: &str) -> String {
    match FormattedBody::markdown(markdown) {
        Some(html) => html.body,
        None => markdown
            .split('\n')
            .map(escaped)
            .collect::<Vec<_>>()
            .join("<br>"),
    }
}

fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Text the composer sends as it is: one beginning with `/` is a command
/// unless it begins with `//`.
fn unslashed(text: String) -> String {
    match text.starts_with('/') {
        true => format!("/{text}"),
        false => text,
    }
}

/// Whether HTML `a` shows as `b` does, but for what no client shows:
/// the elements' other names (`<b>`, `<strong>`), whitespace as HTML
/// collapses it, spaces at the edges of formatting, the newline ending a
/// code block, a line break ending a line that ends anyway, an empty
/// paragraph, a list numbered from 1 as such; and text alongside blocks,
/// or in a quote, as a paragraph, as a message that is one paragraph is
/// its text. Else, it differs in what shows, if only in a paragraph's
/// spacing (of a list's items, say): HTML that Markdown cannot spell.
fn shows_as(a: &str, b: &str) -> bool {
    shown(a) == shown(b)
}

/// HTML, as it shows.
#[derive(Debug, PartialEq)]
enum Node {
    Text(String),
    Element {
        name: String,
        attributes: Vec<(String, String)>,
        children: Vec<Node>,
    },
}

const BLOCKS: &[&str] = &[
    "p",
    "div",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "ul",
    "ol",
    "li",
    "pre",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "caption",
    "hr",
    "details",
    "summary",
];

impl Node {
    fn is(&self, names: &[&str]) -> bool {
        matches!(self, Node::Element { name, .. } if names.contains(&name.as_str()))
    }

    /// Whether spaces at its edges show out of it, as of no element: of
    /// formatting, not of code, whose background they show in.
    fn sheds_spaces(&self) -> bool {
        matches!(self, Node::Element { name, .. }
            if !BLOCKS.contains(&name.as_str()) && !matches!(name.as_str(), "code" | "br" | "img"))
    }
}

fn shown(html: &str) -> Vec<Node> {
    let dom = html5ever::parse_fragment(
        RcDom::default(),
        Default::default(),
        QualName::new(None, ns!(html), local_name!("div")),
        vec![],
        false,
    )
    .one(html);
    // The `<html>` element html5ever puts the fragment's nodes in.
    let root = dom.document.children.borrow()[0].clone();
    let mut nodes = paragraphs(block(children(&root, false)));
    // A message that is one paragraph shows as its text.
    if let [Node::Element { name, children, .. }] = nodes.as_mut_slice() {
        if name == "p" {
            return std::mem::take(children);
        }
    }
    nodes
}

/// The nodes of `handle`, whitespace collapsed but in a `<pre>`.
fn children(handle: &Handle, pre: bool) -> Vec<Node> {
    let mut nodes = vec![];
    for child in handle.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } if !contents.borrow().is_empty() => {
                let text = contents.borrow();
                nodes.push(Node::Text(match pre {
                    true => text.to_string(),
                    false => collapsed(&text),
                }));
            }
            NodeData::Element { name, attrs, .. } => {
                let name = match &*name.local {
                    "b" => "strong",
                    "i" => "em",
                    "s" | "strike" => "del",
                    other => other,
                }
                .to_owned();
                let mut attributes: Vec<(String, String)> = attrs
                    .borrow()
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.to_string()))
                    // A list numbered from 1 is as one numbered from where lists are.
                    .filter(|(attribute, value)| {
                        !(name == "ol" && attribute == "start" && value == "1")
                    })
                    .collect();
                attributes.sort();
                let pre = pre || name == "pre";
                let mut inner = children(child, pre);
                if name == "pre" {
                    trim_final_newline(&mut inner);
                }
                let inner = match (name.as_str(), BLOCKS.contains(&name.as_str()) && !pre) {
                    ("blockquote", _) => paragraphs(block(inner)),
                    (_, true) => block(inner),
                    (_, false) => inline(inner),
                };
                nodes.push(Node::Element {
                    name,
                    attributes,
                    children: inner,
                });
            }
            _ => {}
        }
    }
    nodes
}

/// Text, each run of whitespace as one space, as HTML shows it.
fn collapsed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c.is_ascii_whitespace() {
            true if out.ends_with(' ') => {}
            true => out.push(' '),
            false => out.push(c),
        }
    }
    out
}

/// A code block's last line ends it, with a newline or not.
fn trim_final_newline(nodes: &mut Vec<Node>) {
    match nodes.last_mut() {
        Some(Node::Text(text)) => {
            if text.ends_with('\n') {
                text.pop();
            }
            if text.is_empty() {
                nodes.pop();
            }
        }
        Some(Node::Element { children, .. }) => trim_final_newline(children),
        None => {}
    }
}

/// The nodes of formatting, their spaces at its edges shed out of it,
/// adjacent text joined.
fn inline(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = vec![];
    let push_text = |out: &mut Vec<Node>, text: &str| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(Node::Text(last)) => {
                // Collapsed, as HTML does.
                let text = match last.ends_with(' ') {
                    true => text.trim_start_matches(' '),
                    false => text,
                };
                last.push_str(text);
            }
            _ => out.push(Node::Text(text.to_owned())),
        }
    };
    for node in nodes {
        match node {
            Node::Text(text) => push_text(&mut out, &text),
            mut element if element.sheds_spaces() => {
                let Node::Element { children, .. } = &mut element else {
                    unreachable!()
                };
                let leading = matches!(children.first(), Some(Node::Text(t)) if t.starts_with(' '));
                let trailing = matches!(children.last(), Some(Node::Text(t)) if t.ends_with(' '));
                if leading {
                    let Some(Node::Text(t)) = children.first_mut() else {
                        unreachable!()
                    };
                    *t = t.trim_start_matches(' ').to_owned();
                }
                if trailing {
                    let Some(Node::Text(t)) = children.last_mut() else {
                        unreachable!()
                    };
                    *t = t.trim_end_matches(' ').to_owned();
                }
                children.retain(|c| !matches!(c, Node::Text(t) if t.is_empty()));
                if leading {
                    push_text(&mut out, " ");
                }
                out.push(element);
                if trailing {
                    push_text(&mut out, " ");
                }
            }
            element => out.push(element),
        }
    }
    out
}

/// The nodes of a block: as formatting's, but for spaces at its edges,
/// or next to a block or a line break in it, which show as nothing.
fn block(nodes: Vec<Node>) -> Vec<Node> {
    let mut nodes = inline(nodes);
    let breaks = |node: Option<&Node>| node.is_none_or(|n| n.is(BLOCKS) || n.is(&["br"]));
    for i in 0..nodes.len() {
        let (before, rest) = nodes.split_at_mut(i);
        let (node, after) = rest.split_first_mut().expect("i is in range");
        if let Node::Text(text) = node {
            if breaks(before.last()) {
                *text = text.trim_start_matches(' ').to_owned();
            }
            if breaks(after.first()) {
                *text = text.trim_end_matches(' ').to_owned();
            }
        }
    }
    // Nor does an empty paragraph.
    nodes.retain(|n| match n {
        Node::Text(t) => !t.is_empty(),
        Node::Element { name, children, .. } => !(name == "p" && children.is_empty()),
    });
    // A line break that ends a line ending anyway shows as nothing.
    let mut i = 0;
    while i < nodes.len() {
        let ends = nodes.get(i + 1).is_none_or(|n| n.is(BLOCKS));
        match nodes[i].is(&["br"]) && ends {
            true => {
                nodes.remove(i);
                i = i.saturating_sub(1);
            }
            false => i += 1,
        }
    }
    nodes
}

/// The nodes of the message, or of a quote: its text shows as a
/// paragraph of its own.
fn paragraphs(nodes: Vec<Node>) -> Vec<Node> {
    let mut out = vec![];
    let mut run = vec![];
    let end_run = |run: &mut Vec<Node>, out: &mut Vec<Node>| {
        let children = block(std::mem::take(run));
        if !children.is_empty() {
            out.push(Node::Element {
                name: "p".into(),
                attributes: vec![],
                children,
            });
        }
    };
    for node in nodes {
        match node.is(BLOCKS) {
            true => {
                end_run(&mut run, &mut out);
                out.push(node);
            }
            false => run.push(node),
        }
    }
    end_run(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn html(formatted_body: &str) -> Value {
        json!({
            "msgtype": "m.text",
            "body": "the fallback",
            "format": "org.matrix.custom.html",
            "formatted_body": formatted_body,
        })
    }

    fn edit(formatted_body: &str) -> String {
        edit_text(&html(formatted_body)).unwrap()
    }

    #[test]
    fn a_bridges_formatting_is_edited_as_markdown_that_formats_as_it() {
        // As a Slack bridge sends them: their bodies format as nothing.
        for formatted_body in [
            "<del>struck </del>rest",
            "say <del>(this)</del>now",
            "<strong>bold </strong>text",
            "<del>a</del><del>b</del>",
            "a <em>b</em>c and <code>x*y</code>",
        ] {
            let text = edit(formatted_body);
            assert!(!text.starts_with('/'), "{formatted_body}: {text}");
            assert!(
                shows_as(&formatted(&text), formatted_body),
                "{formatted_body}: {text} => {}",
                formatted(&text)
            );
        }
        assert_eq!(edit("<strong>bold </strong>text"), "**bold** text");
        assert_eq!(
            edit(r#"<a href="https://matrix.to/#/@a:x.org">Alice</a>: <em>hi</em>"#),
            "[Alice](https://matrix.to/#/@a:x.org): *hi*",
            "its links to users mention them again"
        );
    }

    #[test]
    fn text_that_looks_like_markdown_is_escaped() {
        for text in [
            "a literal *star*",
            "snake_case and __init__",
            "typed &lt;b&gt;tag&lt;/b&gt;",
            "1. not a list",
            "# not a heading",
            r"C:\Program Files",
        ] {
            let formatted_body = format!("<p>{text}</p><p>and more</p>");
            let edited = edit(&formatted_body);
            assert!(!edited.starts_with("/html"), "{edited}");
            assert!(shows_as(&formatted(&edited), &formatted_body), "{edited}");
        }
    }

    #[test]
    fn what_markdown_cannot_spell_stays_html() {
        let spoiler = r#"a <span data-mx-spoiler="">secret</span>"#;
        assert!(
            shows_as(&formatted(&edit(spoiler)), spoiler),
            "{}",
            edit(spoiler)
        );
        assert!(edit(spoiler).contains("data-mx-spoiler"));
    }

    #[test]
    fn html_that_markdown_does_not_format_as_is_sent_as_it_is() {
        // A loose list item's paragraph, which Markdown writes tight.
        let html = "<ul><li><p>a</p></li></ul>";
        assert!(
            !shows_as(&formatted(&faithful(html)), html),
            "{}",
            faithful(html)
        );
        assert_eq!(edit(html), format!("/html {html}"));
    }

    #[test]
    fn plain_text_is_edited_as_it_is() {
        let plain = |body: &str| edit_text(&json!({ "msgtype": "m.text", "body": body })).unwrap();
        assert_eq!(plain("just words\nand lines"), "just words\nand lines");
        assert_eq!(plain("**as is**"), "/plain **as is**");
        assert_eq!(plain("/etc/hosts"), "//etc/hosts");
        assert_eq!(
            edit_text(&json!({ "msgtype": "m.emote", "body": "waves" })).as_deref(),
            Some("waves")
        );
        assert_eq!(
            edit_text(&json!({ "msgtype": "m.image", "body": "cat.png" })),
            None
        );
    }

    #[test]
    fn markdown_beginning_with_a_slash_sends_one() {
        assert_eq!(edit("/etc/<em>hosts</em>"), "//etc/*hosts*");
    }

    #[test]
    fn what_no_client_shows_is_no_difference() {
        assert!(shows_as(
            "<b>a</b> <i>b</i> <s>c</s>",
            "<strong>a</strong> <em>b</em> <del>c</del>"
        ));
        assert!(shows_as("<p>a  b\n c</p>", "a b c"));
        assert!(shows_as("<del>a </del>b", "<del>a</del> b"));
        assert!(shows_as("<p>a</p>\n<p>b</p>\n", "<p>a</p><p>b</p>"));
        assert!(shows_as(
            "<pre><code>x\n</code></pre>",
            "<pre><code>x</code></pre>"
        ));
        assert!(shows_as("a <br> b", "a<br>b"));
        // Text alongside blocks, or in a quote, as a paragraph's.
        assert!(shows_as(
            "<h5>t</h5><b>by</b><br><p>x</p>",
            "<h5>t</h5>\n<p><strong>by</strong></p>\n<p>x</p>\n"
        ));
        assert!(shows_as(
            "<blockquote>q</blockquote>",
            "<blockquote>\n<p>q</p>\n</blockquote>\n"
        ));
        assert!(shows_as(
            "a<br><blockquote>q</blockquote><p></p>",
            "<p>a</p><blockquote><p>q</p></blockquote>"
        ));
        assert!(shows_as(
            r#"<ol start="1"><li>a</li></ol>"#,
            "<ol><li>a</li></ol>"
        ));
        assert!(shows_as(
            "<pre><code></code></pre>",
            "<pre><code>\n</code></pre>"
        ));
        // But what shows.
        assert!(!shows_as("<p>a</p><p>b</p>", "a<br>b"));
        assert!(!shows_as("<code> a</code>", "<code>a</code>"));
        assert!(!shows_as("<del>a</del>b", "<del>ab</del>"));
        assert!(!shows_as("<pre>a  b</pre>", "<pre>a b</pre>"));
        assert!(!shows_as(r#"<a href="x">a</a>"#, r#"<a href="y">a</a>"#));
        assert!(!shows_as("<del>a b</del>", "<del>a</del> b"));
        assert!(!shows_as(
            "<ul><li><p>a</p></li></ul>",
            "<ul><li>a</li></ul>"
        ));
        assert!(!shows_as(
            r#"<ol start="2"><li>a</li></ol>"#,
            "<ol><li>a</li></ol>"
        ));
    }

    #[test]
    fn html_sent_is_plain_as_markdown() {
        assert_eq!(
            plain("<b>bold</b> and <del>gone</del>"),
            "**bold** and ~~gone~~"
        );
    }
}
