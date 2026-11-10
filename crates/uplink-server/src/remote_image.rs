//! Mail's remote images, fetched for the client (SPEC §8.9), so that a
//! sender learns of uplink's server instead of the user's browser: its
//! address, and a fixed user agent; none of the browser's headers or
//! cookies. Only public addresses are fetched from: a mail must not make
//! the server reach what only it can (its network, a cloud's metadata).

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect, Url,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

/// What the fetches tell senders of the client: as little as will get
/// images served.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; uplink)";
/// The largest image fetched.
const MAX_BYTES: usize = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

/// Fetches remote images from the addresses `admits` admits.
#[derive(Clone)]
pub struct RemoteImages {
    client: reqwest::Client,
    admits: fn(IpAddr) -> bool,
}

impl RemoteImages {
    /// `admits`: [`public`], but for tests.
    pub fn new(admits: fn(IpAddr) -> bool) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // A proxy would resolve names itself, past `admits`.
            .no_proxy()
            .dns_resolver(Arc::new(Admitted(admits)))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if !fetchable(attempt.url(), admits) {
                    attempt.error("redirected to an address not fetched from")
                } else {
                    attempt.follow()
                }
            }))
            .referer(false)
            .timeout(TIMEOUT)
            .build()
            .expect("the client's settings are valid");
        Self { client, admits }
    }

    /// The image at `url`, as a response never shown by the browser as a
    /// page of uplink's origin, where an SVG one could run its scripts.
    pub async fn get(&self, url: Url) -> Response {
        // Names are judged as they resolve; addresses are not resolved.
        if !fetchable(&url, self.admits) {
            return (StatusCode::FORBIDDEN, "not fetched from").into_response();
        }
        match self.fetch(url).await {
            Ok((mime, image)) => (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                    (header::CONTENT_SECURITY_POLICY, "sandbox".to_owned()),
                    (header::CACHE_CONTROL, "private, max-age=86400".to_owned()),
                ],
                image,
            )
                .into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        }
    }

    /// The image at `url`, and its MIME type.
    async fn fetch(&self, url: Url) -> Result<(String, Vec<u8>), String> {
        let mut response = self
            .client
            .get(url)
            .header(header::ACCEPT, "image/*")
            .send()
            .await
            .map_err(|e| format!("fetching: {:#}", anyhow::Error::from(e)))?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|mime| mime.starts_with("image/"))
            .ok_or("not an image")?
            .to_owned();
        let mut image = vec![];
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("reading: {:#}", anyhow::Error::from(e)))?
        {
            if image.len() + chunk.len() > MAX_BYTES {
                return Err(format!("larger than {} MB", MAX_BYTES >> 20));
            }
            image.extend_from_slice(&chunk);
        }
        Ok((mime, image))
    }
}

/// Whether `ip` is an address of the public internet, as far as the
/// standard library tells (its `is_global` is unstable).
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [first, second, ..] = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_documentation()
                || a.is_unspecified()
                || a.is_multicast()
                || first == 0
                || first >= 240
                // Shared address space (carrier-grade NAT).
                || (first == 100 && second & 0xc0 == 64))
        }
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(a) => public(IpAddr::V4(a)),
            None => {
                let first = a.segments()[0];
                !(a.is_loopback()
                    || a.is_unspecified()
                    || a.is_multicast()
                    // Unique local, and link-local.
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80)
            }
        },
    }
}

/// Whether `url` may be fetched from: HTTP(S), and, if its host is an
/// address, an admitted one. A name's addresses are judged as it is
/// resolved ([`Admitted`]).
fn fetchable(url: &Url, admits: fn(IpAddr) -> bool) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(url::Host::Domain(_)) => true,
            Some(url::Host::Ipv4(a)) => admits(a.into()),
            Some(url::Host::Ipv6(a)) => admits(a.into()),
            None => false,
        }
}

/// Resolves names to their admitted addresses only.
struct Admitted(fn(IpAddr) -> bool);

impl Resolve for Admitted {
    fn resolve(&self, name: Name) -> Resolving {
        let admits = self.0;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| admits(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{} has no address fetched from", name.as_str()).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, routing, Router};

    /// A server of images, a page, a too large image, and redirects, at
    /// 127.0.0.1; it answers with the user agent it was sent, as a header.
    async fn origin() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let echo = |headers: axum::http::HeaderMap, mime: &'static str, body: Vec<u8>| {
            let agent = headers[header::USER_AGENT].to_str().unwrap().to_owned();
            let cookies = headers.contains_key(header::COOKIE).to_string();
            (
                [
                    (header::CONTENT_TYPE, mime.to_owned()),
                    (header::WARNING, format!("{agent}; cookies {cookies}")),
                ],
                body,
            )
        };
        let app = Router::new()
            .route(
                "/a.png",
                routing::get(move |h| async move { echo(h, "image/png", b"PNG".to_vec()) }),
            )
            .route(
                "/page",
                routing::get(move |h| async move { echo(h, "text/html", b"<p>".to_vec()) }),
            )
            .route(
                "/huge.png",
                routing::get(move |h| async move { echo(h, "image/png", vec![0; MAX_BYTES + 1]) }),
            )
            .route(
                "/to-local",
                routing::get(|| async {
                    axum::response::Redirect::temporary("http://10.0.0.1/a.png")
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    async fn get(images: &RemoteImages, url: &str) -> (StatusCode, String, String) {
        let response = images.get(url.parse().unwrap()).await;
        let status = response.status();
        let warning = response
            .headers()
            .get(header::WARNING)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, warning, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn images_are_fetched_as_uplink() {
        let origin = origin().await;
        let images = RemoteImages::new(|_| true);
        let response = images
            .get(format!("http://{origin}/a.png").parse().unwrap())
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], "sandbox");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        // The origin's own header is not passed on.
        assert!(!headers.contains_key(header::WARNING));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            &b"PNG"[..]
        );
    }

    #[tokio::test]
    async fn the_origin_sees_only_uplink() {
        let origin = origin().await;
        let images = RemoteImages::new(|_| true);
        let response = images
            .client
            .get(format!("http://{origin}/a.png"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::WARNING],
            format!("{USER_AGENT}; cookies false")
        );
    }

    #[tokio::test]
    async fn only_images_are_fetched() {
        let origin = origin().await;
        let images = RemoteImages::new(|_| true);
        assert_eq!(
            get(&images, &format!("http://{origin}/page")).await,
            (
                StatusCode::BAD_GATEWAY,
                String::new(),
                "not an image".into()
            )
        );
        assert_eq!(
            get(&images, &format!("http://{origin}/huge.png")).await,
            (
                StatusCode::BAD_GATEWAY,
                String::new(),
                "larger than 16 MB".into()
            )
        );
        assert_eq!(
            get(&images, "ftp://example.com/a.png").await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn only_public_addresses_are_fetched_from() {
        let origin = origin().await;
        let images = RemoteImages::new(public);
        assert_eq!(
            get(&images, &format!("http://{origin}/a.png")).await.0,
            StatusCode::FORBIDDEN
        );
        for url in [
            format!("http://localhost:{}/a.png", origin.port()),
            "http://[::ffff:127.0.0.1]/a.png".to_owned(),
        ] {
            let (status, _, body) = get(&images, &url).await;
            assert!(
                matches!(status, StatusCode::FORBIDDEN | StatusCode::BAD_GATEWAY),
                "{url}: {status} {body}"
            );
        }
        // Nor through a redirect from a public one.
        let images = RemoteImages::new(|ip| ip.is_loopback());
        let (status, _, body) = get(&images, &format!("http://{origin}/to-local")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body.contains("redirected to an address not fetched from"),
            "{body}"
        );
    }

    #[test]
    fn public_addresses() {
        for ip in [
            "93.184.215.14",
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c",
            "100.128.0.1",
        ] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "240.0.0.1",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "ff02::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
    }
}
