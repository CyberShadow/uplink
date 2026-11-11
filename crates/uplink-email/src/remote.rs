//! A mail's remote images, made to come through uplink's server (SPEC
//! §8.9). The HTML is parsed into a DOM (by html5ever, as ammonia and
//! browsers parse it) and its CSS into tokens (by cssparser), and URLs
//! are rewritten there, never in text: what is rewritten is what a browser
//! would have fetched. Anything left unrecognized is blocked by the
//! frame's CSP, not fetched directly.

use cssparser::{ParseError, Parser, Token};
use html5ever::{
    local_name, ns,
    serialize::{serialize, SerializeOpts},
    tendril::TendrilSink,
    QualName,
};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};

/// Where uplink's server fetches a remote image for a message's HTML body,
/// given its URL as `url`.
pub const REMOTE_IMAGE_PATH: &str = "/api/remote-image";

/// The proxied form of a remote image's URL, if `url` is one (`http:`,
/// `https:`, or scheme-relative, taken as `https:`).
fn proxied(url: &str) -> Option<String> {
    let url = url.trim();
    let absolute = match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_owned(),
    };
    let parsed = url::Url::parse(&absolute).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| {
        let encoded: String =
            url::form_urlencoded::byte_serialize(parsed.as_str().as_bytes()).collect();
        format!("{REMOTE_IMAGE_PATH}?url={encoded}")
    })
}

/// `html` (a fragment, as ammonia takes it) with its remote images
/// proxied: `<img src>`, and URLs in CSS, of `style` attributes and
/// `<style>` elements. Other `<img>` sources go, but for the message's own
/// (`cid:`), which sanitizing inlines.
pub fn proxy_html(html: &str) -> String {
    // As ammonia parses: a fragment, as if in a <div>.
    let dom = html5ever::parse_fragment(
        RcDom::default(),
        Default::default(),
        QualName::new(None, ns!(html), local_name!("div")),
        vec![],
        false,
    )
    .one(html);
    let root = dom.document.children.borrow()[0].clone();
    proxy_node(&root);
    let mut out = vec![];
    serialize(
        &mut out,
        &SerializableHandle::from(root),
        SerializeOpts::default(),
    )
    .expect("writing to a Vec succeeds");
    String::from_utf8(out).expect("html5ever writes UTF-8")
}

fn proxy_node(node: &Handle) {
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        attrs.retain_mut(|attr| {
            match (&*name.local, &*attr.name.local) {
                ("img", "src") if attr.value.starts_with("cid:") => {}
                ("img", "src") => match proxied(&attr.value) {
                    Some(url) => attr.value = url.into(),
                    None => return false,
                },
                (_, "style") => attr.value = proxy_css(&attr.value).into(),
                _ => {}
            }
            true
        });
        if name.local == local_name!("style") {
            for child in node.children.borrow().iter() {
                if let NodeData::Text { contents } = &child.data {
                    let css = proxy_css(&contents.borrow());
                    *contents.borrow_mut() = css.into();
                }
            }
        }
    }
    for child in node.children.borrow().iter() {
        proxy_node(child);
    }
}

/// `css` with the URLs of its resources proxied if remote, and made void
/// if neither remote nor `data:` (relative ones would load from uplink).
/// The CSS is tokenized as a browser does, escapes and all; only the
/// tokens of URLs change, and the rest is kept as written. CSS nested
/// deeper than cssparser goes (75 blocks) goes whole: what it has not
/// read, it cannot vouch for.
pub fn proxy_css(css: &str) -> String {
    let mut parser = Parser::new(css);
    let mut edits = vec![];
    if urls(&mut parser, false, &mut edits).is_err() {
        return String::new();
    }
    let mut out = String::with_capacity(css.len());
    let mut copied = 0;
    for (start, end, replacement) in edits {
        out.push_str(&css[copied..start]);
        out.push_str(&replacement);
        copied = end;
    }
    out.push_str(&css[copied..]);
    out
}

/// A replacement of the source's bytes `start..end`.
type Edit = (usize, usize, String);

/// What a resource's URL becomes, as a quoted CSS string. Its `<` are
/// escaped: the URL is as cssparser read it, escapes undone, and a
/// `data:` one written back as is could close the `<style>` element it is
/// in (`url("data:\3C/style>…")`), its rest then read as HTML, never
/// rewritten.
fn replaced(url: &str) -> String {
    let url = match proxied(url) {
        Some(url) => url,
        None if url.trim_start().starts_with("data:") => url.to_owned(),
        None => String::new(),
    };
    let mut quoted = String::new();
    cssparser::serialize_string(&url, &mut quoted).expect("writing to a String succeeds");
    quoted.replace('<', "\\3c ")
}

/// Too deeply nested to read.
struct TooDeep;

/// Collects the edits to the URLs from `parser` on: `url(…)` and `src(…)`,
/// and quoted strings where they are URLs: in `image-set(…)` (when
/// `image_set`), and right after `@import`.
fn urls(parser: &mut Parser, image_set: bool, edits: &mut Vec<Edit>) -> Result<(), TooDeep> {
    let mut after_import = false;
    loop {
        let start = parser.position().byte_index();
        let Ok(token) = parser.next_including_whitespace_and_comments() else {
            return Ok(());
        };
        let token = token.clone();
        if matches!(token, Token::WhiteSpace(_) | Token::Comment(_)) {
            continue;
        }
        let string_is_url = image_set || std::mem::take(&mut after_import);
        match token {
            Token::UnquotedUrl(url) => {
                let end = parser.position().byte_index();
                edits.push((start, end, format!("url({})", replaced(&url))));
            }
            Token::QuotedString(url) if string_is_url => {
                let end = parser.position().byte_index();
                edits.push((start, end, replaced(&url)));
            }
            Token::Function(name)
                if name.eq_ignore_ascii_case("url") || name.eq_ignore_ascii_case("src") =>
            {
                // A quoted URL, perhaps with modifiers, which go.
                let url = nested(parser, |block| {
                    let url = block
                        .expect_string()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    while block.next_including_whitespace_and_comments().is_ok() {}
                    Ok(url)
                })?;
                let end = parser.position().byte_index();
                edits.push((start, end, format!("url({})", replaced(&url))));
            }
            Token::Function(name) => {
                let image_set = name.to_ascii_lowercase().ends_with("image-set");
                nested(parser, |block| urls(block, image_set, edits))?
            }
            Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                nested(parser, |block| urls(block, false, edits))?
            }
            Token::AtKeyword(name) if name.eq_ignore_ascii_case("import") => after_import = true,
            _ => {}
        }
    }
}

/// `read` of the block just opened. It reads to the block's end, so the
/// only error is nesting too deep, its own or cssparser's limit.
fn nested<'i, T>(
    parser: &mut Parser<'i>,
    read: impl FnOnce(&mut Parser<'i>) -> Result<T, TooDeep>,
) -> Result<T, TooDeep> {
    parser
        .parse_nested_block(|block| {
            read(block).map_err(|TooDeep| ParseError {
                kind: cssparser::ParseErrorKind::Custom(()),
            })
        })
        .map_err(|_: ParseError<()>| TooDeep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(url: &str) -> String {
        format!(
            "{REMOTE_IMAGE_PATH}?url={}",
            url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
        )
    }

    #[test]
    fn css_urls_are_found_as_a_browser_finds_them() {
        let a = proxy("http://x.example/a%20b.png");
        let b = proxy("https://x.example/b.png");
        let f = proxy("https://x.example/f.png");
        for (css, expected) in [
            // Quoted, unquoted, spaced, in any case.
            (
                "td { background: URL( 'http://x.example/a b.png' ) }",
                format!("td {{ background: url(\"{a}\") }}"),
            ),
            ("i{background:url(https://x.example/b.png)}", format!("i{{background:url(\"{b}\")}}")),
            ("i { background: url(//x.example/b.png) }", format!("i {{ background: url(\"{b}\") }}")),
            // Escapes, in the function's name and in the URL.
            ("i { background: \\75 rl(https://x.example/b.png) }", format!("i {{ background: url(\"{b}\") }}")),
            ("i { background: url(https://x.example/\\66.png) }", format!("i {{ background: url(\"{f}\") }}")),
            // Modifiers go.
            ("i { background: url(\"https://x.example/b.png\" crossorigin) }", format!("i {{ background: url(\"{b}\") }}")),
            // Strings that are URLs.
            (
                "i { background: image-set(\"https://x.example/b.png\" 1x, url(https://x.example/f.png) 2x) }",
                format!("i {{ background: image-set(\"{b}\" 1x, url(\"{f}\") 2x) }}"),
            ),
            ("@import 'https://x.example/b.png'; i { content: 'https://x' }", format!("@import \"{b}\"; i {{ content: 'https://x' }}")),
            // In nested rules.
            ("@media screen { i { background: url(https://x.example/b.png) } }", format!("@media screen {{ i {{ background: url(\"{b}\") }} }}")),
            // `data:` stays; anything else is void, relative URLs most of
            // all: they would be fetched from uplink.
            ("i { background: url(data:image/png;base64,AA==) }", "i { background: url(\"data:image/png;base64,AA==\") }".into()),
            ("i { background: url(/api/remote-image?url=x) }", "i { background: url(\"\") }".into()),
            ("i { background: url(cid:a) }", "i { background: url(\"\") }".into()),
            // Nor does `data:` close the `<style>` it is in, once its
            // escapes are undone.
            ("i { background: url('data:\\3C/style>') }", "i { background: url(\"data:\\3c /style>\") }".into()),
            // The rest is as written, comments and all.
            ("p { color: red } /* url(https://x) */", "p { color: red } /* url(https://x) */".into()),
        ] {
            assert_eq!(proxy_css(css), expected, "{css}");
        }
    }

    #[test]
    fn css_too_deep_to_read_goes() {
        let deep = format!("{}url(https://x.example/a.png)", "a(".repeat(100));
        assert_eq!(proxy_css(&deep), "");
        let shallow = format!("{}url(https://x.example/a.png)", "a(".repeat(10));
        assert!(proxy_css(&shallow).contains(REMOTE_IMAGE_PATH));
    }

    #[test]
    fn html_is_rewritten_where_a_browser_would_fetch() {
        let html = proxy_html(
            "<style>i { background: url(https://x.example/b.png) }</style>\
             <p style='background: url(https://x.example/b.png)'>url(https://x.example/b.png) \
             <img alt='<style>url(https://x.example/b.png)</style>' src='https://x.example/b.png'>\
             <img src='/relative.png'><img src='cid:own'><img src='javascript:x()'></p>",
        );
        let b = proxy("https://x.example/b.png");
        assert_eq!(
            html,
            format!(
                "<style>i {{ background: url(\"{b}\") }}</style>\
                 <p style=\"background: url(&quot;{b}&quot;)\">url(https://x.example/b.png) \
                 <img alt=\"&lt;style&gt;url(https://x.example/b.png)&lt;/style&gt;\" src=\"{b}\">\
                 <img><img src=\"cid:own\"><img></p>"
            )
        );
    }
}
