//! Mail's remote images, fetched for the client (SPEC §8.9), so that a
//! sender learns of uplink's server instead of the user's browser: its
//! address, and a fixed user agent; none of the browser's headers or
//! cookies. Only public addresses are fetched from: a mail must not make
//! the server reach what only it can (its network, a cloud's metadata);
//! but for the hosts the user names (`remote_images.private_hosts`).

use arc_swap::ArcSwap;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect, Url,
};
use std::{
    collections::HashSet,
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
    /// Names fetched from whatever they resolve to, as [`host_name`] has
    /// them; shared with the resolver, and swapped as the config changes.
    private_hosts: Arc<ArcSwap<HashSet<String>>>,
}

impl RemoteImages {
    /// `admits`: [`public`], but for tests.
    pub fn new(admits: fn(IpAddr) -> bool) -> Self {
        let private_hosts = Arc::new(ArcSwap::from_pointee(HashSet::new()));
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // A proxy would resolve names itself, past `admits`.
            .no_proxy()
            .dns_resolver(Arc::new(Admitted {
                admits,
                private_hosts: private_hosts.clone(),
            }))
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
        Self {
            client,
            admits,
            private_hosts,
        }
    }

    /// The hosts fetched from though their addresses are not public, as
    /// of the next connection: one open to a host taken off may serve
    /// until it idles out.
    pub fn set_private_hosts(&self, hosts: &[String]) {
        self.private_hosts
            .store(Arc::new(hosts.iter().map(|h| host_name(h)).collect()));
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
            .map_err(|e| {
                let e = anyhow::Error::from(e);
                // Said alone: within reqwest's chain it reads as DNS failing.
                match e.chain().find_map(|e| e.downcast_ref::<NotAdmitted>()) {
                    Some(refused) => refused.to_string(),
                    None => format!("fetching: {e:#}"),
                }
            })?
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

/// Whether `ip` is an address of the public internet, as IANA's
/// special-purpose registries have it (the standard library's
/// `is_global` is unstable). An IPv6 address that carries an IPv4 one
/// (mapped, compatible, NAT64's, 6to4's) is judged by that: through a
/// translator or a tunnel, it is the IPv4 address that is reached.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [first, second, third, _] = a.octets();
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
                || (first == 100 && second & 0xc0 == 64)
                // IETF protocol assignments.
                || (first == 192 && second == 0 && third == 0)
                // Benchmarking, which proxies' "fake IP" DNS hands out.
                || (first == 198 && second & 0xfe == 18))
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            let embedded = |high: u16, low: u16| {
                public(IpAddr::V4(
                    ((u32::from(high) << 16) | u32::from(low)).into(),
                ))
            };
            match s {
                // IPv4-mapped and IPv4-compatible (:: and ::1 among them).
                [0, 0, 0, 0, 0, 0 | 0xffff, ..] => embedded(s[6], s[7]),
                // NAT64's well-known prefix.
                [0x64, 0xff9b, 0, 0, 0, 0, ..] => embedded(s[6], s[7]),
                // 6to4.
                [0x2002, ..] => embedded(s[1], s[2]),
                _ => {
                    !(a.is_multicast()
                        // Unique local, link-local, and site-local.
                        || s[0] & 0xfe00 == 0xfc00
                        || s[0] & 0xffc0 == 0xfe80
                        || s[0] & 0xffc0 == 0xfec0
                        // NAT64's local-use prefix.
                        || (s[0] == 0x64 && s[1] == 0xff9b)
                        // Discard-only.
                        || (s[0] == 0x100 && s[1..4] == [0, 0, 0])
                        // IETF protocol assignments (Teredo among them),
                        // and documentation.
                        || (s[0] == 0x2001 && s[1] < 0x200)
                        || (s[0] == 0x2001 && s[1] == 0xdb8))
                }
            }
        }
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

/// A name that resolved to addresses none of which are fetched from.
#[derive(Debug)]
struct NotAdmitted {
    name: String,
    refused: Vec<IpAddr>,
}

impl std::fmt::Display for NotAdmitted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.refused.is_empty() {
            return write!(f, "{} has no address", self.name);
        }
        let refused: Vec<String> = self.refused.iter().map(IpAddr::to_string).collect();
        write!(
            f,
            "{} resolves only to addresses not of the public internet ({}), \
             which images are not fetched from but for remote_images.private_hosts",
            self.name,
            refused.join(", ")
        )
    }
}

impl std::error::Error for NotAdmitted {}

/// A host name as compared: lowercase, without a trailing dot.
fn host_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Resolves names to their admitted addresses only, but for the private
/// hosts, whose addresses are all admitted.
struct Admitted {
    admits: fn(IpAddr) -> bool,
    private_hosts: Arc<ArcSwap<HashSet<String>>>,
}

impl Resolve for Admitted {
    fn resolve(&self, name: Name) -> Resolving {
        let admits = self.admits;
        let private = self
            .private_hosts
            .load()
            .contains(&host_name(name.as_str()));
        Box::pin(async move {
            let (addrs, refused): (Vec<SocketAddr>, Vec<SocketAddr>) =
                tokio::net::lookup_host((name.as_str(), 0))
                    .await?
                    .partition(|a| private || admits(a.ip()));
            if addrs.is_empty() {
                let mut refused: Vec<IpAddr> = refused.iter().map(SocketAddr::ip).collect();
                refused.dedup();
                return Err(NotAdmitted {
                    name: name.as_str().to_owned(),
                    refused,
                }
                .into());
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
    async fn names_of_no_public_address_are_said_to_be() {
        let images = RemoteImages::new(public);
        let response = images
            .get("http://localhost:1/a.png".parse().unwrap())
            .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.starts_with("localhost resolves only to addresses not of the public internet (")
                && body.contains("127.0.0.1")
                && body.ends_with(
                    "), which images are not fetched from but for remote_images.private_hosts"
                ),
            "{body}"
        );
    }

    /// A private host is fetched from at its private addresses, named in
    /// any case; others, and its addresses named themselves, are not.
    #[tokio::test]
    async fn private_hosts_are_fetched_from() {
        let origin = origin().await;
        let images = RemoteImages::new(public);
        images.set_private_hosts(&["LocalHost.".to_owned()]);
        let at_localhost = || format!("http://localhost:{}/a.png", origin.port());
        let response = images.get(at_localhost().parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = images
            .get(format!("http://{origin}/a.png").parse().unwrap())
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Taken off the list, it is not fetched from: a client's own (one
        // already connected may reuse its connection until idle).
        let images = RemoteImages::new(public);
        images.set_private_hosts(&["localhost".to_owned()]);
        images.set_private_hosts(&[]);
        let response = images.get(at_localhost().parse().unwrap()).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
            "198.20.0.1",
            "::ffff:93.184.215.14",
            "64:ff9b::5db8:d70e",
            "2002:5db8:d70e::1",
            "2001:4860:4860::8888",
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
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.255",
            // IPv4 within IPv6, as a translator or a tunnel reaches it.
            "::127.0.0.1",
            "::10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
            "64:ff9b:1::1",
            "fec0::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
    }
}
