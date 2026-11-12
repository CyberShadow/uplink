//! A mail's remote images, made to come through uplink's server (SPEC
//! §8.9). The HTML is parsed into a DOM (by html5ever, as ammonia and
//! browsers parse it) and its CSS into tokens (by cssparser), and URLs
//! are rewritten there, never in text: what is rewritten is what a browser
//! would have fetched. Anything left unrecognized is blocked by the
//! frame's CSP, not fetched directly.

use cssparser::{Delimiter, ParseError, Parser, Token};
use html5ever::{
    local_name, ns,
    serialize::{serialize, SerializeOpts},
    tendril::TendrilSink,
    Attribute, QualName,
};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};
use std::collections::BTreeMap;

/// Where uplink's server fetches a remote image for a message's HTML body:
/// `/api/remote-image/<host>?url=<URL>`, the host its URL's, so that the
/// frame's CSP can let a host's images through by path.
pub const REMOTE_IMAGE_PATH: &str = "/api/remote-image";

/// Where a likely tracking pixel's URL points instead, as at
/// [`REMOTE_IMAGE_PATH`]: where the frame's CSP never lets it through, and
/// the server serves nothing. Its markup stays.
pub const PIXEL_PATH: &str = "/api/remote-pixel";

/// A mail's remote images: how many each host has, but for likely
/// tracking pixels, which are only counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remote {
    pub hosts: BTreeMap<String, u32>,
    pub pixels: u32,
}

/// A remote image's URL, if `url` is one (`http:`, `https:`, or
/// scheme-relative, taken as `https:`), with its host: a name of DNS's
/// letters, digits, `-`, `_` and `.`, or an IPv4 address. Its host is in
/// the path it is fetched by, and in the frame's CSP as it is shown: an
/// IPv6 address has no place in a path, and URLs take hosts of `;`, `,`,
/// `'` or `"` (escaped or not), which would end the CSP's directives or
/// the attribute it is in.
pub fn remote(url: &str) -> Option<(String, url::Url)> {
    let url = url.trim();
    let absolute = match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_owned(),
    };
    let parsed = url::Url::parse(&absolute).ok()?;
    let host = match parsed.host()? {
        url::Host::Domain(host)
            if host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) =>
        {
            host.to_owned()
        }
        url::Host::Domain(_) | url::Host::Ipv6(_) => return None,
        url::Host::Ipv4(ip) => ip.to_string(),
    };
    matches!(parsed.scheme(), "http" | "https").then_some((host, parsed))
}

/// The proxied form of a remote image's URL, if `url` is one, at `path`
/// ([`REMOTE_IMAGE_PATH`] or [`PIXEL_PATH`]), counted in `remote`.
fn proxied(url: &str, path: &'static str, remote_images: &mut Remote) -> Option<String> {
    let (host, parsed) = remote(url)?;
    let encoded: String =
        url::form_urlencoded::byte_serialize(parsed.as_str().as_bytes()).collect();
    let proxied = format!("{path}/{host}?url={encoded}");
    if path == PIXEL_PATH {
        remote_images.pixels += 1;
    } else {
        *remote_images.hosts.entry(host).or_default() += 1;
    }
    Some(proxied)
}

/// `html` (a fragment, as ammonia takes it) with its remote images
/// proxied: `<img src>`, and URLs in CSS, of `style` attributes and
/// `<style>` elements; and what they are. Other `<img>` sources go, but
/// for the message's own (`cid:`), which sanitizing inlines. Likely
/// tracking pixels ([`pixel`]) point at [`PIXEL_PATH`].
pub fn proxy_html(html: &str) -> (String, Remote) {
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
    let mut remote = Remote::default();
    proxy_node(&root, &mut remote);
    let mut out = vec![];
    serialize(
        &mut out,
        &SerializableHandle::from(root),
        SerializeOpts::default(),
    )
    .expect("writing to a Vec succeeds");
    (
        String::from_utf8(out).expect("html5ever writes UTF-8"),
        remote,
    )
}

fn proxy_node(node: &Handle, remote: &mut Remote) {
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        let path = match name.local == local_name!("img") && pixel(&attrs) {
            true => PIXEL_PATH,
            false => REMOTE_IMAGE_PATH,
        };
        attrs.retain_mut(|attr| {
            match (&*name.local, &*attr.name.local) {
                ("img", "src") if attr.value.starts_with("cid:") => {}
                ("img", "src") => match proxied(&attr.value, path, remote) {
                    Some(url) => attr.value = url.into(),
                    None => return false,
                },
                (_, "style") => attr.value = proxy_css_of(&attr.value, remote).into(),
                _ => {}
            }
            true
        });
        if name.local == local_name!("style") {
            for child in node.children.borrow().iter() {
                if let NodeData::Text { contents } = &child.data {
                    let css = proxy_css_of(&contents.borrow(), remote);
                    *contents.borrow_mut() = css.into();
                }
            }
        }
    }
    for child in node.children.borrow().iter() {
        proxy_node(child, remote);
    }
}

/// Whether an `<img>` of `attrs` is likely a tracking pixel: too small to
/// see (both sides at most 2px, or one, the other not given), or hidden
/// by its `style`. Only its markup tells: fetching it to see would be
/// what tracks. Wrongly so, nothing is missed: its box stays, empty, as
/// all but empty it would have been.
fn pixel(attrs: &[Attribute]) -> bool {
    let attr = |name: &str| {
        attrs
            .iter()
            .find(|a| &*a.name.local == name)
            .map(|a| a.value.to_string())
    };
    let style = attr("style").map(|css| sized(&css)).unwrap_or_default();
    let side = |name: &str, of_style: Option<f32>| {
        of_style.or_else(|| {
            let value = attr(name)?;
            let value = value.trim();
            value
                .strip_suffix("px")
                .unwrap_or(value)
                .trim()
                .parse()
                .ok()
        })
    };
    let width = side("width", style.width);
    let height = side("height", style.height);
    let small = |side: Option<f32>| side.is_some_and(|s| s <= 2.0);
    style.hidden
        || (small(width) && (small(height) || height.is_none()))
        || (small(height) && width.is_none())
}

/// What a `style` attribute says of an image's box.
#[derive(Debug, Default, PartialEq)]
struct Sized {
    /// In px.
    width: Option<f32>,
    height: Option<f32>,
    /// `display: none`, `visibility: hidden`, `opacity: 0`, or a maximum
    /// width or height of 0.
    hidden: bool,
}

fn sized(css: &str) -> Sized {
    let mut parser = Parser::new(css);
    let mut sized = Sized::default();
    while !parser.is_exhausted() {
        // A declaration it cannot read, it skips, as a browser does.
        let _: Result<(), ParseError<()>> =
            parser.parse_until_after(Delimiter::Semicolon, |declaration| {
                let name = declaration.expect_ident()?.to_ascii_lowercase();
                declaration.expect_colon()?;
                let value = declaration.next()?.clone();
                let px = match &value {
                    Token::Dimension { value, unit, .. } if unit.eq_ignore_ascii_case("px") => {
                        Some(*value)
                    }
                    Token::Number { value, .. } if *value == 0.0 => Some(0.0),
                    _ => None,
                };
                let zero = match &value {
                    Token::Number { value, .. } | Token::Dimension { value, .. } => *value == 0.0,
                    Token::Percentage { unit_value, .. } => *unit_value == 0.0,
                    _ => false,
                };
                let is =
                    |word: &str| matches!(&value, Token::Ident(v) if v.eq_ignore_ascii_case(word));
                match &*name {
                    "width" => sized.width = px,
                    "height" => sized.height = px,
                    "display" if is("none") => sized.hidden = true,
                    "visibility" if is("hidden") => sized.hidden = true,
                    "opacity" | "max-width" | "max-height" if zero => sized.hidden = true,
                    _ => {}
                }
                Ok(())
            });
    }
    sized
}

/// `css` with the URLs of its resources proxied if remote, and made void
/// if neither remote nor `data:` (relative ones would load from uplink).
/// The CSS is tokenized as a browser does, escapes and all; only the
/// tokens of URLs change, and the rest is kept as written. CSS nested
/// deeper than cssparser goes (75 blocks) goes whole: what it has not
/// read, it cannot vouch for.
pub fn proxy_css(css: &str) -> String {
    proxy_css_of(css, &mut Remote::default())
}

/// [`proxy_css`], counting the images in `remote`.
fn proxy_css_of(css: &str, remote: &mut Remote) -> String {
    let mut parser = Parser::new(css);
    let mut edits = vec![];
    if urls(&mut parser, false, &mut edits, remote).is_err() {
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
fn replaced(url: &str, remote: &mut Remote) -> String {
    let url = match proxied(url, REMOTE_IMAGE_PATH, remote) {
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
fn urls(
    parser: &mut Parser,
    image_set: bool,
    edits: &mut Vec<Edit>,
    remote: &mut Remote,
) -> Result<(), TooDeep> {
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
                edits.push((start, end, format!("url({})", replaced(&url, remote))));
            }
            Token::QuotedString(url) if string_is_url => {
                let end = parser.position().byte_index();
                edits.push((start, end, replaced(&url, remote)));
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
                edits.push((start, end, format!("url({})", replaced(&url, remote))));
            }
            Token::Function(name) => {
                let image_set = name.to_ascii_lowercase().ends_with("image-set");
                nested(parser, |block| urls(block, image_set, edits, remote))?
            }
            Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                nested(parser, |block| urls(block, false, edits, remote))?
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
        let host = remote(url).unwrap().0;
        format!(
            "{REMOTE_IMAGE_PATH}/{host}?url={}",
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
            ("i { background: url(/api/remote-image/x.example?url=x) }", "i { background: url(\"\") }".into()),
            ("i { background: url(cid:a) }", "i { background: url(\"\") }".into()),
            // An IPv6 host has no place in a path, nor one that would end
            // the frame's CSP's directives, or its attribute.
            ("i { background: url(https://[::1]/a.png) }", "i { background: url(\"\") }".into()),
            ("i { background: url('https://a;font-src *;.example/a.png') }", "i { background: url(\"\") }".into()),
            ("i { background: url(https://a%22b.example/a.png) }", "i { background: url(\"\") }".into()),
            ("i { background: url(https://a,b.example/a.png) }", "i { background: url(\"\") }".into()),
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
        let (html, remote) = proxy_html(
            "<style>i { background: url(https://x.example/b.png) }</style>\
             <p style='background: url(https://x.example/b.png)'>url(https://x.example/b.png) \
             <img alt='<style>url(https://x.example/b.png)</style>' src='https://X.example/b.png'>\
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
        assert_eq!(
            remote,
            Remote {
                hosts: [("x.example".to_owned(), 3)].into(),
                pixels: 0
            }
        );
    }

    #[test]
    fn likely_tracking_pixels_point_where_they_never_load() {
        for (img, is_pixel) in [
            ("width=1 height=1", true),
            ("width='2px' height=' 1 '", true),
            ("width=1", true),
            ("height=0", true),
            ("style='width: 1px; height: 1px'", true),
            ("style='display:none'", true),
            ("style='visibility: HIDDEN'", true),
            ("style='opacity: 0'", true),
            ("style='max-height: 0px'", true),
            ("width=600 style='display: block'", false),
            // Its style over its attributes, as a browser sizes it.
            ("width=1 height=1 style='width: 100px'", false),
            ("style='height: 1px; width: 1px' width=300", true),
            // Too small to see one way only: a line, seen.
            ("width=600 height=1", false),
            ("width=50%", false),
            ("", false),
            // What the style says, it says even ill-formed elsewhere.
            ("style='color: ; display: none'", true),
        ] {
            let (html, remote) = proxy_html(&format!("<img {img} src='https://t.example/p.gif'>"));
            let path = if is_pixel {
                PIXEL_PATH
            } else {
                REMOTE_IMAGE_PATH
            };
            assert!(
                html.contains(&format!("src=\"{path}/t.example?url=")),
                "{img}: {html}"
            );
            assert_eq!(remote.pixels, u32::from(is_pixel), "{img}");
            assert_eq!(remote.hosts.len(), usize::from(!is_pixel), "{img}");
        }
    }
}
