//! Previews of the links messages carry (SPEC §9.8), as the homeserver
//! makes them (`/preview_url`): what the page says of itself (OpenGraph),
//! and its image, which the homeserver uploads.

use anyhow::{Context, Result};
use matrix_sdk::{
    config::RequestConfig, ruma::api::client::authenticated_media::get_media_preview, Client,
};
use uplink_protocol::matrix::UrlPreview;

/// Whether one of a message's contents (its own, and its edits') links
/// `url`, a web page: so that the client cannot have the homeserver fetch
/// whatever it names.
pub fn linked(contents: &[serde_json::Value], url: &str) -> bool {
    let web = url.starts_with("https://") || url.starts_with("http://");
    web && contents.iter().any(|content| {
        ["body", "formatted_body"].iter().any(|field| {
            content[field]
                .as_str()
                .is_some_and(|text| text.contains(url))
        })
    })
}

/// The page's preview, of the homeserver's OpenGraph data: `None` if it
/// says nothing of the page.
pub fn preview(og: &serde_json::Value) -> Option<UrlPreview> {
    let text = |key: &str| {
        og[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // Sizes come as numbers, or as strings of them.
    let size = |key: &str| match &og[key] {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    };
    let preview = UrlPreview {
        title: text("og:title"),
        description: text("og:description"),
        site_name: text("og:site_name"),
        image: text("og:image").filter(|image| image.starts_with("mxc://")),
        image_width: size("og:image:width"),
        image_height: size("og:image:height"),
    };
    (preview.title.is_some() || preview.description.is_some()).then_some(preview)
}

/// The homeserver's OpenGraph data of `url`. Its answer is final: it
/// fetched the page itself, so an error (a 502 of a page it could not
/// reach) is not one to retry, as the SDK otherwise would, for up to a
/// quarter of an hour.
pub async fn fetch(client: &Client, url: &str) -> Result<serde_json::Value> {
    let response = client
        .send(get_media_preview::v1::Request::new(url.to_owned()))
        .with_request_config(RequestConfig::new().disable_retry())
        .await
        .with_context(|| format!("previewing {url}"))?;
    Ok(match response.data {
        Some(data) => serde_json::from_str(data.get())?,
        None => serde_json::json!({}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_web_pages_the_message_links_are_previewed() {
        let contents = [
            json!({ "body": "see https://example.org/a" }),
            json!({ "body": "* edited", "formatted_body": "<a href=\"https://example.org/b\">b</a>" }),
        ];
        assert!(linked(&contents, "https://example.org/a"));
        assert!(linked(&contents, "https://example.org/b"), "an edit's");
        assert!(!linked(&contents, "https://example.org/c"));
        let file = [json!({ "body": "file:///etc/passwd" })];
        assert!(!linked(&file, "file:///etc/passwd"));
    }

    #[test]
    fn previews_say_what_the_page_says_of_itself() {
        let og = json!({
            "og:title": " Rust ",
            "og:description": "A language",
            "og:image": "mxc://hs/abc",
            "og:image:width": 1200,
            "og:image:height": "630",
            "matrix:image:size": 1000,
        });
        assert_eq!(
            preview(&og),
            Some(UrlPreview {
                title: Some("Rust".into()),
                description: Some("A language".into()),
                site_name: None,
                image: Some("mxc://hs/abc".into()),
                image_width: Some(1200),
                image_height: Some(630),
            })
        );
        // An image not uploaded is none; a page that says nothing, no preview.
        let og = json!({ "og:title": "t", "og:image": "https://x/i.png" });
        assert_eq!(preview(&og).unwrap().image, None);
        assert_eq!(
            preview(&json!({ "og:image": "mxc://hs/abc", "og:title": "" })),
            None
        );
    }
}
