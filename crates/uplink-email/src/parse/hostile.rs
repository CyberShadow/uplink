//! Hostile HTML mail, against what [`html_body`] promises the client
//! (SPEC §8.9): its output, parsed as the browser parses it, has no
//! scripts or handlers, nothing that navigates or submits by itself, and
//! nothing that fetches but `data:` URIs and uplink's fetches of remote
//! images. The checks read the output on their own, with html5ever and
//! cssparser, rather than trusting the rewriting they check.
//!
//! Remote resources are on `t.example`: in the output, that name may
//! appear in links (`<a href>`) and text only, or encoded within a URL
//! of uplink's fetches.

use super::html_body;
use crate::remote::{remote, PIXEL_PATH, REMOTE_IMAGE_PATH};
use cssparser::{Parser, Token};
use html5ever::{local_name, ns, tendril::TendrilSink, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Elements the output may have: ammonia's defaults, and mail's own.
const ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "area",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "map",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "rtc",
    "ruby",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "th",
    "thead",
    "time",
    "tfoot",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// The hostile bodies, each a way past one layer or another.
const HOSTILE: &[&str] = &[
    // Scripts and handlers, plainly and obfuscated.
    "<script>alert(1)</script><SCRIPT SRC=https://t.example/x.js></SCRIPT>",
    "<img src=x onerror=alert(1)><body onload=alert(1)><svg onload=alert(1)>",
    "<a href=\"javascript:alert(1)\">a</a><a href=\"JaVaScRiPt:alert(1)\">b</a>\
     <a href=\"java&#x09;script:alert(1)\">c</a><a href=\" &#14; javascript:alert(1)\">d</a>\
     <a href=\"vbscript:x\">e</a><a href=\"data:text/html,<script>alert(1)</script>\">f</a>",
    "<iframe src=\"https://t.example/f\"></iframe><frame src=https://t.example/f>\
     <object data=\"https://t.example/o\"></object><embed src=\"https://t.example/e\">\
     <applet code=x></applet><portal src=https://t.example/p></portal>",
    // Parser differentials (mXSS): raw text, namespaces, comments.
    "<noscript><p title=\"</noscript><img src=https://t.example/n onerror=alert(1)>\"></noscript>",
    "<svg><style><img src=https://t.example/s onerror=alert(1)></style></svg>",
    "<math><mtext><table><mglyph><style><img src=https://t.example/m onerror=alert(1)>",
    "<math><mi><malignmark><style><!--</style><img src=https://t.example/m2 onerror=alert(1)>",
    "<select><style></select><img src=https://t.example/sel onerror=alert(1)></style>",
    "<!--><img src=https://t.example/c1 onerror=alert(1)>--><!--!><img src=https://t.example/c2>-->",
    "<![CDATA[><img src=https://t.example/cd onerror=alert(1)>]]>",
    "<xmp><img src=https://t.example/xmp></xmp><noembed><img src=https://t.example/ne></noembed>\
     <noframes><img src=https://t.example/nf></noframes><textarea><img src=https://t.example/ta></textarea>",
    "<template><img src=https://t.example/tpl onerror=alert(1)></template>",
    // A `<style>` closed from within its CSS, by an escape the rewriting
    // undoes.
    "<style>i{background:url(\"data:\\3C/style><img src='https://t.example/b1'>\")}</style>",
    "<style>i{background:url(\"data:\\3c/STYLE><img src='/api/messages/1/attachments/0'>\")}</style>",
    "<style>@import \"data:\\3C/style><img src=https://t.example/b2>\";</style>",
    "<style>i{background:image-set(\"data:\\3C/style><img src=https://t.example/b3>\" 1x)}</style>",
    "<p style=\"background:url(&quot;data:\\3C/style>&lt;img src=https://t.example/b4>&quot;)\">p</p>",
    // Navigation and forms, without scripts.
    "<meta http-equiv=refresh content=\"0;url=https://t.example/r\">\
     <base href=\"https://t.example/\"><link rel=stylesheet href=https://t.example/l>\
     <link rel=prefetch href=https://t.example/pf><link rel=dns-prefetch href=//t.example>",
    "<form action=\"https://t.example/f\"><input name=a><button formaction=https://t.example/g>b</button>\
     <input type=image src=https://t.example/i></form><isindex action=https://t.example/x>",
    "<a href=\"https://t.example/ok\" ping=\"https://t.example/ping\" target=_top download>a</a>\
     <area href=https://t.example/area ping=https://t.example/ping2>",
    // Images by other means than `<img src>`.
    "<img srcset=\"https://t.example/ss 1x\" src=\"https://t.example/ok.png\" lowsrc=https://t.example/lo \
     dynsrc=https://t.example/dyn longdesc=https://t.example/ld>",
    "<picture><source srcset=\"https://t.example/src\"><img></picture>\
     <video poster=\"https://t.example/v\" src=\"https://t.example/v2\"><track src=https://t.example/t></video>\
     <audio src=\"https://t.example/a\"></audio>",
    "<table background=\"https://t.example/tb\"><tr><td background=\"https://t.example/td\">x</td></tr></table>\
     <body background=\"https://t.example/body\"><div style=\"background:url(https://t.example/div)\"></div>",
    "<svg><image href=\"https://t.example/si\"/><use href=\"https://t.example/u#a\"/>\
     <feImage href=https://t.example/fe /></svg>",
    "<img src=\"/api/remote-image/t.example?url=https://t.example/self\"><img src=\"//t.example/rel\">\
     <img src=\"cid:none\"><img src=\"http:t.example/opaque\"><img src=\"\\\\t.example\\share\">",
    // CSS's ways to fetch.
    "<style>@import url(https://t.example/i1); @import 'https://t.example/i2'; \
     @import \"//t.example/i3\" screen; @namespace url(https://t.example/ns);</style>",
    "<style>@font-face{font-family:x;src:url(https://t.example/font) format('woff'),local(x)}\
     i{cursor:url(https://t.example/cur),auto;list-style:url(https://t.example/ls);\
     border-image:url(https://t.example/bi) 30;mask:url(https://t.example/mk);\
     content:url(https://t.example/ct);filter:url(https://t.example/flt#f);\
     shape-outside:url(https://t.example/so);offset-path:url(https://t.example/op)}</style>",
    "<style>i{background:\\75 rl(https://t.example/e1)}b{background:u\\72 l(https://t.example/e2)}\
     s{background:url(https://t\\2e example/e3)}q{background:URL(  \"https://t.example/e4\"  )}</style>",
    "<style>i{background:image-set('https://t.example/is1' 1x,url(//t.example/is2) 2x)}\
     b{background:-webkit-image-set('https://t.example/is3' 1x)}\
     s{--v:url(https://t.example/var);background:var(--v)}\
     q{background:src(\"https://t.example/srcfn\")}</style>",
    "<style>@media screen{@supports (display:grid){@layer x{@container (min-width:1px){\
     i{background:url(https://t.example/nest)}}}}}</style>",
    "<style>@keyframes k{from{background:url(https://t.example/k1)}to{background:url(https://t.example/k2)}}\
     i:hover{background:url(https://t.example/hover)}a:visited{background:url(https://t.example/visited)}</style>",
    "<style>i{background:url(https://t.example/a\\\n)} b{background:url(https://t.example/bad url)}</style>",
    "<style>/*</style><img src=https://t.example/cmt>*/</style>",
    "<p style=\"behavior:url(https://t.example/htc);-moz-binding:url(https://t.example/xbl#x);\
     background:url(&#34;https://t.example/ent&#34;)\">p</p>",
    "<style>i{background:url(/api/remote-image/t.example?url=https://t.example/selfcss)}\
     b{background:url(/api/messages/1/attachments/0)}</style>",
    // Nested deeper than cssparser reads.
    concat!(
        "<style>",
        "a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(",
        "a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(a(",
        "url(https://t.example/deep)</style>"
    ),
    // Hosts that, in the frame's CSP as shown, would end its directives or
    // the attribute it is in.
    "<img src=\"https://t.example;font-src *;.t.example/csp\">\
     <img src=\"https://t.example%22%3E%3Cimg%20src=x.t.example/quote\">\
     <i style=\"background:url('https://t.example,*.t.example/comma')\">i</i>",
    // Clobbering, and the frame's own names.
    "<img name=getElementById><form id=x name=y><a id=defaultView name=location>a</a>\
     <div class=uplink-quote id=top>q</div>",
];

#[test]
fn hostile_html_is_contained() {
    let mut failures = vec![];
    for &body in HOSTILE {
        let raw = format!("From: a@example.com\r\nContent-Type: text/html\r\n\r\n{body}");
        let html = html_body(raw.as_bytes()).expect("an HTML body").0;
        let mut problems = vec![];
        let dom = parse(&html);
        // The `<html>` element html5ever puts the fragment's nodes in.
        let root = dom.document.children.borrow()[0].clone();
        assert!(
            !root.children.borrow().is_empty() || html.is_empty(),
            "{html}"
        );
        for node in root.children.borrow().iter() {
            check_node(node, &mut problems);
        }
        if !problems.is_empty() {
            failures.push(format!("{body}\n => {html}\n{problems:#?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `html` as the client's frame parses it: in a `<body>`. (Dropping the
/// DOM empties its nodes.)
fn parse(html: &str) -> RcDom {
    html5ever::parse_fragment(
        RcDom::default(),
        Default::default(),
        QualName::new(None, ns!(html), local_name!("body")),
        vec![],
        false,
    )
    .one(html)
}

fn check_node(node: &Handle, problems: &mut Vec<String>) {
    match &node.data {
        NodeData::Element { name, attrs, .. } => {
            let element = &*name.local;
            if name.ns != ns!(html) || !ELEMENTS.contains(&element) {
                problems.push(format!("element {:?} {element}", name.ns));
            }
            for attr in attrs.borrow().iter() {
                let (attribute, value) = (&*attr.name.local, &*attr.value);
                match attribute {
                    a if a.starts_with("on") => problems.push(format!("handler {a}")),
                    "style" => check_css(value, problems),
                    "src" if element == "img" => {
                        if !(value.starts_with("data:") || fetched(value)) {
                            problems.push(format!("img src {value}"));
                        }
                    }
                    // Links of the web's schemes; relative ones open
                    // uplink, in a tab of its own.
                    "href" => match value.split_once(':') {
                        Some((scheme, _))
                            if !scheme.contains('/')
                                && !["http", "https", "mailto"]
                                    .contains(&&*scheme.to_lowercase()) =>
                        {
                            problems.push(format!("href {value}"))
                        }
                        _ => {}
                    },
                    "rel" if element == "a" => {
                        if value != "noopener noreferrer" {
                            problems.push(format!("rel {value}"));
                        }
                    }
                    a => {
                        if [
                            "srcset",
                            "background",
                            "poster",
                            "action",
                            "formaction",
                            "ping",
                            "target",
                            "data",
                            "lowsrc",
                            "dynsrc",
                            "longdesc",
                            "name",
                            "src",
                            "http-equiv",
                            "download",
                            "xlink:href",
                        ]
                        .contains(&a)
                            || value.contains("t.example")
                        {
                            problems.push(format!("attribute {a}={value}"));
                        }
                    }
                }
            }
            if element == "style" {
                for child in node.children.borrow().iter() {
                    match &child.data {
                        NodeData::Text { contents } => check_css(&contents.borrow(), problems),
                        _ => problems.push("non-text in <style>".into()),
                    }
                }
            }
        }
        NodeData::Comment { .. } | NodeData::ProcessingInstruction { .. } => {
            problems.push("comment".into())
        }
        _ => {}
    }
    for child in node.children.borrow().iter() {
        check_node(child, problems);
    }
}

/// Whether `url` is of uplink's fetches of remote images (or of pixels,
/// never fetched): at the path of the host of the URL it fetches.
fn fetched(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix(REMOTE_IMAGE_PATH)
        .or_else(|| url.strip_prefix(PIXEL_PATH))
    else {
        return false;
    };
    let Some((host, encoded)) = rest.strip_prefix('/').and_then(|r| r.split_once("?url=")) else {
        return false;
    };
    let decoded: String = url::form_urlencoded::parse(format!("u={encoded}").as_bytes())
        .map(|(_, v)| v.into_owned())
        .collect();
    host.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !encoded.contains(['/', ':', '<', '"', '\''])
        && remote(&decoded).is_some_and(|(of, _)| of == host)
}

/// Every URL-like value of `css`, as a browser tokenizes it, is void,
/// `data:` or of uplink's fetches; and `t.example` is nowhere but there.
fn check_css(css: &str, problems: &mut Vec<String>) {
    check_tokens(&mut Parser::new(css), problems);
}

fn check_tokens(parser: &mut Parser, problems: &mut Vec<String>) {
    let ok = |value: &str| value.is_empty() || value.starts_with("data:") || fetched(value);
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
        let token = token.clone();
        match &token {
            Token::UnquotedUrl(url) | Token::QuotedString(url) if !ok(url) => {
                if url.contains("t.example") || matches!(token, Token::UnquotedUrl(_)) {
                    problems.push(format!("CSS URL {url:?}"));
                }
            }
            // A bad URL (`Token::BadUrl`) is never fetched: it stays.
            Token::Ident(value) if value.contains("t.example") => {
                problems.push(format!("CSS {value:?}"))
            }
            Token::Comment(value) if value.contains("t.example") => {
                problems.push(format!("CSS {value:?}"))
            }
            _ => {}
        }
        let is_url = matches!(&token, Token::Function(f) if f.eq_ignore_ascii_case("url") || f.eq_ignore_ascii_case("src"));
        if matches!(
            token,
            Token::Function(_)
                | Token::ParenthesisBlock
                | Token::SquareBracketBlock
                | Token::CurlyBracketBlock
        ) {
            let _ = parser.parse_nested_block(|block| -> Result<(), cssparser::ParseError<()>> {
                if is_url {
                    if let Ok(url) = block.expect_string_cloned() {
                        if !ok(&url) {
                            problems.push(format!("CSS url() {url:?}"));
                        }
                    }
                }
                check_tokens(block, problems);
                Ok(())
            });
        }
    }
}
