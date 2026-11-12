//! Hostile `formatted_body`s, against what [`sanitized`] promises the
//! client (SPEC §9.2): its output, parsed as the browser parses it in the
//! page it is put in, has only the elements and attributes the Matrix
//! spec has clients show, no handlers, and loads nothing but the event's
//! own media, by URLs the browser takes as they are; its links are of
//! the web's schemes, or of that media. The checks read the output on
//! their own, with html5ever and the url crate, rather than trusting the
//! sanitizing they check.
//!
//! Remote resources are on `t.example`: in the output, that name may
//! appear in links (`<a href>`) and text only.

use super::{sanitized, TAGS};
use html5ever::{local_name, ns, tendril::TendrilSink, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde_json::json;
use uplink_protocol::matrix::{RoomEventId, RoomId};

const PREFIX: &str = "/api/rooms/3/events/7/media/";

/// Where uplink is, for the browser's resolving of relative URLs.
const ORIGIN: &str = "https://uplink.test";

const HOSTILE: &[&str] = &[
    // Dot-segments, which browsers resolve, out of the event's media to
    // anything of uplink's: its fetches of mail's remote images most of
    // all.
    r#"<img src="mxc://x/../../../../../remote-image/t.example?url=https%3A%2F%2Ft.example%2Fdots">"#,
    r#"<img src="mxc://../..">"#,
    r#"<img src="mxc://./x">"#,
    r#"<img src="mxc://x/%2e%2e/%2e%2e/%2e%2e/%2e%2e/%2e%2e/remote-image/t.example?url=https://t.example/enc">"#,
    r#"<img src="mxc://x/.%2E/.%2e/%2E.">"#,
    r"<img src='mxc://x\..\..\..\..\..\remote-image/t.example?url=https://t.example/backslash'>",
    "<img src=\"mxc://x/.\t./.\n./..\">",
    r#"<img src="mxc&#58;//x/&#46;&#46;/&#46;&#46;/&#46;&#46;/&#46;&#46;/&#46;&#46;/blobs">"#,
    r#"<img src="mxc://x.org/cat?../../../../..#x">"#,
    r#"<img src="mxc://x.org:8448/cat/../../../..">"#,
    // Relative URLs, as if already served.
    r#"<img src="/api/rooms/3/events/7/media/x/../../../../../remote-image/t.example?url=https://t.example/rel">"#,
    r#"<img src="/api/rooms/3/events/7/media/x/cat/../../../../..">"#,
    r#"<img src="/api/rooms/3/events/7/media/x/cat?url=https://t.example/query">"#,
    r#"<img src="/api/rooms/3/events/7/media/x%2Fcat">"#,
    r#"<a href="/api/rooms/3/events/7/media/../../../../../blobs/z">rel</a>"#,
    r#"<a href="/api/rooms/3/events/7/media/x/..%2f..%2f..">rel</a>"#,
    r##"<a href="/api/blobs/z">blob</a><a href="?x">q</a><a href="#x">h</a><a href="">e</a><a href=".">d</a>"##,
    // Of other hosts, as written or as browsers read them.
    r#"<img src="https://t.example/abs.png"><img src="//t.example/proto.png">"#,
    r#"<img src="\\t.example\back.png"><img src="/\t.example/mixed.png"><img src="https:t.example/noslash.png">"#,
    r#"<a href="//t.example/proto">p</a><a href="\\t.example">b</a><a href="/\t.example">m</a>"#,
    // Schemes that run, or show what the sender wrote as uplink's.
    r#"<a href="javascript:alert(1)">j</a><a href="JaVaScRiPt:alert(1)">J</a>"#,
    "<a href=\"java\tscript:alert(1)\">t</a><a href=\" javascript:alert(1)\">s</a>",
    r#"<a href="&#x6a;avascript:alert(1)">e</a><a href="data:text/html,<script>alert(1)</script>">d</a>"#,
    r#"<a href="vbscript:x">v</a><a href="blob:https://uplink.test/x">b</a><a href="mxc://x/cat">m</a>"#,
    r#"<img src="data:image/svg+xml,<svg onload=alert(1)>"><img src="javascript:alert(1)">"#,
    // Handlers, scripts, styles, and what loads by itself.
    r#"<img src=x onerror="alert(1)"><b onclick="x()">b</b><details open ontoggle="alert(1)">d</details>"#,
    r#"<script>alert(1)</script><style>*{background:url(https://t.example/style)}</style>"#,
    r#"<p style="background:url(https://t.example/attr)">p</p><font color="red" style="x">f</font>"#,
    r#"<link rel=stylesheet href="https://t.example/link.css"><base href="https://t.example/">"#,
    r#"<meta http-equiv=refresh content="0;url=https://t.example/refresh">"#,
    r#"<iframe src="https://t.example/frame"></iframe><object data="https://t.example/o"></object>"#,
    r#"<embed src="https://t.example/e"><video src="https://t.example/v" poster="https://t.example/p"></video>"#,
    r#"<form action="https://t.example/form"><input name=q><button formaction="https://t.example/b">s</button></form>"#,
    r#"<img srcset="https://t.example/srcset 1x"><picture><source srcset="https://t.example/source"></picture>"#,
    r#"<svg><image href="https://t.example/svg"/></svg><math><mi xlink:href="https://t.example/math">m</mi></math>"#,
    // Parser differentials.
    r#"<noscript><p title="</noscript><img src=x onerror=alert(1)>"></noscript>"#,
    r#"<svg><style><img src=x onerror=alert(1)></style></svg>"#,
    r#"<math><mtext><table><mglyph><style><img src=x onerror=alert(1)>"#,
    r#"<!--><img src=x onerror=alert(1)>--><table><td><img src=https://t.example/table>"#,
    r#"<mx-reply><img src=x onerror=alert(1)></mx-reply><mx-reply><a href="javascript:x">r</a>"#,
    // Classes the page's would match, and clobbering of its names.
    r#"<code class="language-x pill">c</code><span class="pill">s</span><div class="x">d</div>"#,
    r#"<img name=getElementById id=defaultView><a id=location name=body>a</a><form id=x>f</form>"#,
    // Colours, that the client applies as styles.
    r##"<span data-mx-color="red;background:url(https://t.example/c)" data-mx-bg-color="#fff">s</span>"##,
];

#[test]
fn hostile_html_is_contained() {
    let mut failures = vec![];
    for &body in HOSTILE {
        let content = json!({ "format": "org.matrix.custom.html", "formatted_body": body });
        let html = sanitized(RoomId(3), RoomEventId(7), &content).unwrap();
        let mut problems = vec![];
        let dom = parse(&html);
        let root = dom.document.children.borrow()[0].clone();
        for node in root.children.borrow().iter() {
            check_node(node, &mut problems);
        }
        if !problems.is_empty() {
            failures.push(format!("{body}\n => {html}\n{problems:#?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The sanitizing keeps the event's media, as it should.
#[test]
fn the_events_media_are_kept() {
    let content = json!({
        "format": "org.matrix.custom.html",
        "formatted_body": r#"<img src="mxc://x.org/cat"><a href="https://t.example/">link</a>"#,
    });
    let html = sanitized(RoomId(3), RoomEventId(7), &content).unwrap();
    let mut problems = vec![];
    for node in parse(&html).document.children.borrow()[0]
        .children
        .borrow()
        .iter()
    {
        check_node(node, &mut problems);
    }
    assert!(problems.is_empty(), "{problems:?}");
    assert!(
        html.contains(r#"src="/api/rooms/3/events/7/media/x.org/cat""#),
        "{html}"
    );
}

/// `html` as the client puts it in its page: in a `<span>`.
fn parse(html: &str) -> RcDom {
    html5ever::parse_fragment(
        RcDom::default(),
        Default::default(),
        QualName::new(None, ns!(html), local_name!("span")),
        vec![],
        false,
    )
    .one(html)
}

fn check_node(node: &Handle, problems: &mut Vec<String>) {
    match &node.data {
        NodeData::Element { name, attrs, .. } => {
            let element = &*name.local;
            if name.ns != ns!(html) || !TAGS.contains(&element) {
                problems.push(format!("element {:?} {element}", name.ns));
            }
            for attr in attrs.borrow().iter() {
                let (attribute, value) = (&*attr.name.local, &*attr.value);
                match (element, attribute) {
                    ("img", "src") => {
                        if !media(value) {
                            problems.push(format!("img src {value}"));
                        }
                    }
                    ("a", "href") => {
                        if !(media(value) || web(value)) {
                            problems.push(format!("a href {value}"));
                        }
                    }
                    ("a", "target") if value == "_blank" => {}
                    ("a", "rel") if value == "noopener noreferrer" => {}
                    ("code", "class") if value.starts_with("language-") && !value.contains(' ') => {
                    }
                    ("img", "width" | "height" | "alt" | "title" | "data-mx-emoticon")
                    | ("font", "color" | "data-mx-color" | "data-mx-bg-color")
                    | (
                        "span",
                        "data-mx-color" | "data-mx-bg-color" | "data-mx-spoiler" | "data-mx-maths",
                    )
                    | ("ol", "start")
                    | ("div", "data-mx-maths") => {}
                    _ => problems.push(format!("attribute {element} {attribute}={value:?}")),
                }
                if element == "img" && value.contains("t.example") {
                    problems.push(format!("img {attribute} {value}"));
                }
            }
            for child in node.children.borrow().iter() {
                check_node(child, problems);
            }
        }
        NodeData::Text { .. } => {}
        _ => problems.push(format!("node {:?}", node.data)),
    }
}

/// Whether `url` is of the event's media, as the browser resolves it in
/// uplink's page: under its prefix, by a server and an id, as written.
fn media(url: &str) -> bool {
    let base = url::Url::parse(&format!("{ORIGIN}/mail")).unwrap();
    let Ok(resolved) = base.join(url) else {
        return false;
    };
    let path = &resolved[url::Position::BeforePath..];
    resolved.origin() == base.origin()
        && path == url
        && path
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(server, id)| {
                !server.is_empty()
                    && !matches!(server, "." | "..")
                    && !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
}

/// Whether `url` is absolute, of a scheme of the web's, or mail's.
fn web(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    url.starts_with(&format!("{}:", parsed.scheme()))
        && matches!(
            parsed.scheme(),
            "http" | "https" | "ftp" | "mailto" | "magnet"
        )
}
