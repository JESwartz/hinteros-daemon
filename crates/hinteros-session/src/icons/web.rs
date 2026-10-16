//! Icons of web apps: find a site's own icon on the web and download it.
//!
//! From a `site_url`: fetch the page, read its `<link>` tags (web app manifest, apple-touch-icon,
//! favicon), read the manifest's `icons`, and take the best candidate (the largest square icon
//! of at least 192 px, preferring ones not made only for masking), falling back to
//! `/favicon.ico`. From an `icon_url`: just that file, and only if its SHA-256 is the pinned
//! one, when the catalog gives one. Only https is used (also for every redirect), requests have
//! timeouts, responses are limited to [`MAX_BYTES`] and must be an image (checked by
//! Content-Type and by the file's first bytes).

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use url::Url;

/// Largest response (page, manifest or image) we read.
pub const MAX_BYTES: u64 = 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
/// How many candidates to try before giving up.
const MAX_TRIES: usize = 5;
/// Icons at least this large (square) are preferred.
pub const WANTED_SIZE: u32 = 192;

/// Where a web app's icon comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// This image; with a hex SHA-256, only if the file has it.
    IconUrl(String, Option<String>),
    /// Discover the icon from this page.
    Site(String),
}

/// A downloaded, checked image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub bytes: Vec<u8>,
    /// File extension: `png`, `svg`, `ico`, `jpg`, `gif` or `webp`.
    pub ext: &'static str,
}

#[derive(Clone)]
pub struct Fetcher {
    agent: ureq::Agent,
    allow_http: bool,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Fetcher {
    /// `allow_http` only for tests against a local server.
    pub fn new(allow_http: bool) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(8)))
            .max_redirects(0)
            .https_only(!allow_http)
            .http_status_as_error(false)
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) HinterOS")
            .build()
            .into();
        Self { agent, allow_http }
    }

    pub fn fetch_icon(&self, source: &Source) -> Result<Image> {
        match source {
            Source::IconUrl(url, None) => self.fetch_image(&self.parse(url)?),
            Source::IconUrl(url, Some(sha256)) => {
                let image = self.fetch_image(&self.parse(url)?)?;
                let got = sha256_hex(&image.bytes);
                if !got.eq_ignore_ascii_case(sha256) {
                    warn!(url, want = %sha256, got = %got, "icon refused: its SHA-256 is not the catalog's");
                    bail!("{url}: SHA-256 {got} is not the pinned {sha256}");
                }
                Ok(image)
            }
            Source::Site(url) => self.discover(&self.parse(url)?),
        }
    }

    fn parse(&self, url: &str) -> Result<Url> {
        let url = Url::parse(url).with_context(|| format!("bad URL {url:?}"))?;
        self.check_scheme(&url)?;
        Ok(url)
    }

    fn check_scheme(&self, url: &Url) -> Result<()> {
        ensure!(
            url.scheme() == "https" || (self.allow_http && url.scheme() == "http"),
            "{url}: only https is allowed"
        );
        Ok(())
    }

    /// GET with manual redirects (https only), a size limit and the final URL.
    fn get(&self, url: &Url) -> Result<(Url, Option<String>, Vec<u8>)> {
        let mut url = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            self.check_scheme(&url)?;
            let mut resp = self
                .agent
                .get(url.as_str())
                .call()
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .with_context(|| format!("{url}: redirect without Location"))?;
                url = url.join(location).context("bad redirect target")?;
                continue;
            }
            ensure!(status == 200, "GET {url}: HTTP {status}");
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                });
            let mut body = Vec::new();
            resp.body_mut()
                .as_reader()
                .take(MAX_BYTES + 1)
                .read_to_end(&mut body)
                .with_context(|| format!("reading {url}"))?;
            ensure!(
                body.len() as u64 <= MAX_BYTES,
                "{url}: larger than {MAX_BYTES} bytes"
            );
            return Ok((url, content_type, body));
        }
        bail!("too many redirects")
    }

    fn fetch_image(&self, url: &Url) -> Result<Image> {
        let (final_url, content_type, bytes) = self.get(url)?;
        let ct = content_type.unwrap_or_default();
        ensure!(
            ct.starts_with("image/") || ct == "application/octet-stream",
            "{final_url}: Content-Type {ct:?} is not an image"
        );
        let ext =
            sniff(&bytes).with_context(|| format!("{final_url}: not a known image format"))?;
        Ok(Image { bytes, ext })
    }

    fn discover(&self, page: &Url) -> Result<Image> {
        let (page, _, html) = self.get(page).context("fetching the page")?;
        let html = String::from_utf8_lossy(&html);
        let mut candidates = html_candidates(&html, &page);
        if let Some(manifest) = manifest_url(&html, &page) {
            match self
                .get(&manifest)
                .and_then(|(u, _, body)| manifest_candidates(&body, &u))
            {
                Ok(c) => candidates.extend(c),
                Err(e) => debug!("web app manifest {manifest}: {e:#}"),
            }
        }
        if let Ok(fav) = page.join("/favicon.ico") {
            candidates.push(Candidate::new(fav, Origin::Fallback));
        }
        rank(&mut candidates);
        let mut last = None;
        for c in candidates.iter().take(MAX_TRIES) {
            if self.check_scheme(&c.url).is_err() {
                continue;
            }
            match self.fetch_image(&c.url) {
                Ok(image) => return Ok(image),
                Err(e) => {
                    debug!("icon candidate {}: {e:#}", c.url);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("{page}: no icon candidates")))
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 64 hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The image format of `bytes`, from its first bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Some("ico")
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
        (head.trim_start().starts_with("<?xml") || head.trim_start().starts_with("<svg"))
            .then_some("svg")
            .filter(|_| head.contains("<svg"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Fallback,
    LinkIcon,
    AppleTouch,
    Manifest,
}

/// An icon the page offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: Url,
    /// Largest square size in pixels (`any` counts as 512); `None` if not stated.
    pub size: Option<u32>,
    /// A manifest icon whose purpose is only `maskable` (it has padding for masks).
    pub maskable_only: bool,
    pub origin: Origin,
}

impl Candidate {
    fn new(url: Url, origin: Origin) -> Self {
        Self {
            url,
            size: None,
            maskable_only: false,
            origin,
        }
    }

    fn score(&self) -> (u8, bool, u32, Origin) {
        let tier = match (self.size, self.origin) {
            (Some(s), _) if s >= WANTED_SIZE => 3,
            // Apple's default touch icon is 180 px.
            (None, Origin::AppleTouch) => 2,
            (None, Origin::Fallback) => 0,
            (None, _) => 1,
            (Some(_), _) => 1,
        };
        (
            tier,
            !self.maskable_only,
            self.size.unwrap_or(0),
            self.origin,
        )
    }
}

/// Best candidate first; stable for ties (page order).
pub fn rank(candidates: &mut [Candidate]) {
    candidates.sort_by_key(|c| std::cmp::Reverse(c.score()));
}

/// `sizes="48x48 192x192"` / `any`: the largest square size.
fn parse_sizes(sizes: &str) -> Option<u32> {
    sizes
        .split_whitespace()
        .filter_map(|t| {
            if t.eq_ignore_ascii_case("any") {
                return Some(512);
            }
            let (w, h) = t.split_once(['x', 'X'])?;
            let (w, h) = (w.parse::<u32>().ok()?, h.parse::<u32>().ok()?);
            (w == h).then_some(w)
        })
        .max()
}

/// The `<link>` tags of an HTML document: attributes as lowercase name/value pairs.
fn link_tags(html: &str) -> Vec<Vec<(String, String)>> {
    let lower = html.to_ascii_lowercase();
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<link") {
        let start = from + i + 5;
        from = start;
        if !lower[start..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        // Attribute values may contain '>' only in odd cases; this is a best-effort scanner.
        tags.push(attributes(&html[start..start + len]));
        from = start + len;
    }
    tags
}

fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let name = s[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = s[vs..i].to_string();
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = s[vs..i].to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

fn attr<'a>(tag: &'a [(String, String)], name: &str) -> Option<&'a str> {
    tag.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Icons named by the page's `<link rel="icon" | "apple-touch-icon" ...>` tags.
pub fn html_candidates(html: &str, page: &Url) -> Vec<Candidate> {
    link_tags(html)
        .iter()
        .filter_map(|tag| {
            let rel = attr(tag, "rel")?.to_ascii_lowercase();
            let rels: Vec<&str> = rel.split_whitespace().collect();
            let origin = if rels.iter().any(|r| r.starts_with("apple-touch-icon")) {
                Origin::AppleTouch
            } else if rels.contains(&"icon") {
                Origin::LinkIcon
            } else {
                return None;
            };
            let url = page.join(attr(tag, "href")?).ok()?;
            let mut c = Candidate::new(url, origin);
            c.size = attr(tag, "sizes").and_then(parse_sizes);
            if c.size.is_none() && origin == Origin::LinkIcon {
                // An SVG icon scales to anything.
                if attr(tag, "type").is_some_and(|t| t.contains("svg"))
                    || c.url.path().ends_with(".svg")
                {
                    c.size = Some(512);
                }
            }
            Some(c)
        })
        .collect()
}

/// The page's `<link rel="manifest">`.
pub fn manifest_url(html: &str, page: &Url) -> Option<Url> {
    link_tags(html).iter().find_map(|tag| {
        let rel = attr(tag, "rel")?.to_ascii_lowercase();
        rel.split_whitespace()
            .any(|r| r == "manifest")
            .then(|| page.join(attr(tag, "href")?).ok())
            .flatten()
    })
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    icons: Vec<ManifestIcon>,
}

#[derive(Deserialize)]
struct ManifestIcon {
    src: String,
    #[serde(default)]
    sizes: String,
    #[serde(default, rename = "type")]
    mime: String,
    #[serde(default)]
    purpose: String,
}

/// Icons listed in a web app manifest fetched from `manifest_url`.
pub fn manifest_candidates(json: &[u8], manifest_url: &Url) -> Result<Vec<Candidate>> {
    let manifest: Manifest = serde_json::from_slice(json).context("bad manifest JSON")?;
    Ok(manifest
        .icons
        .into_iter()
        .filter_map(|i| {
            let mut c = Candidate::new(manifest_url.join(&i.src).ok()?, Origin::Manifest);
            c.size = parse_sizes(&i.sizes);
            if c.size.is_none() && (i.mime.contains("svg") || c.url.path().ends_with(".svg")) {
                c.size = Some(512);
            }
            let purposes: Vec<&str> = i.purpose.split_whitespace().collect();
            c.maskable_only = !purposes.is_empty() && !purposes.contains(&"any");
            Some(c)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-png";

    /// A tiny HTTP server for fixtures: path -> (status, headers, body). Records requests.
    struct Server {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    type Route = (u16, Vec<(&'static str, String)>, Vec<u8>);

    impl Server {
        fn start(routes: impl FnOnce(&str) -> HashMap<&'static str, Route>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes = routes(&base);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) <= 2 {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    log.lock().unwrap().push(path.clone());
                    let (status, headers, body) = routes.get(path.as_str()).cloned().unwrap_or((
                        404,
                        vec![],
                        b"nope".to_vec(),
                    ));
                    let mut head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self { base, requests }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        fn requested(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn ok(content_type: &str, body: &[u8]) -> Route {
        (
            200,
            vec![("Content-Type", content_type.into())],
            body.to_vec(),
        )
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_sizes("48x48 192x192"), Some(192));
        assert_eq!(parse_sizes("any"), Some(512));
        assert_eq!(parse_sizes("192x96"), None);
        assert_eq!(parse_sizes(""), None);
    }

    #[test]
    fn finds_link_tags_in_messy_html() {
        let html = r#"<!DOCTYPE html><HTML><head>
            <LINK REL="Shortcut Icon" href='/fav.png' sizes="32x32">
            <link rel=apple-touch-icon href=/touch.png>
            <link rel="manifest" href="/site.webmanifest"/>
            <link rel="stylesheet" href="/x.css">
            <linkx rel="icon" href="/no.png">
            </head>"#;
        let page = u("https://example.com/watch/");
        let c = html_candidates(html, &page);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].url.as_str(), "https://example.com/fav.png");
        assert_eq!(c[0].size, Some(32));
        assert_eq!(c[1].origin, Origin::AppleTouch);
        assert_eq!(
            manifest_url(html, &page).unwrap().as_str(),
            "https://example.com/site.webmanifest"
        );
    }

    #[test]
    fn ranks_large_square_unmasked_icons_first() {
        let page = u("https://example.com/");
        let manifest = br#"{"icons":[
            {"src":"/m48.png","sizes":"48x48","type":"image/png"},
            {"src":"/m512-mask.png","sizes":"512x512","purpose":"maskable"},
            {"src":"/m192.png","sizes":"192x192","purpose":"any maskable"},
            {"src":"/m256.png","sizes":"256x256"},
            {"src":"/wide.png","sizes":"512x256"}]}"#;
        let mut c = manifest_candidates(manifest, &page).unwrap();
        c.extend(html_candidates(
            r#"<link rel="icon" href="/f.ico"><link rel="apple-touch-icon" href="/t.png">"#,
            &page,
        ));
        c.push(Candidate::new(
            u("https://example.com/favicon.ico"),
            Origin::Fallback,
        ));
        rank(&mut c);
        let order: Vec<&str> = c.iter().map(|c| c.url.path()).collect();
        assert_eq!(
            order,
            [
                "/m256.png",
                "/m192.png",
                "/m512-mask.png",
                "/t.png",
                "/m48.png",
                "/wide.png",
                "/f.ico",
                "/favicon.ico"
            ]
        );
    }

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff(PNG), Some("png"));
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?><svg xmlns=\"x\"/>"),
            Some("svg")
        );
        assert_eq!(sniff(b"<svg/>"), Some("svg"));
        assert_eq!(sniff(b"<html><body>hi</body></html>"), None);
        assert_eq!(sniff(b"\0\0\x01\0\x01\0"), Some("ico"));
    }

    #[test]
    fn discovers_the_manifest_icon_through_the_page() {
        let server = Server::start(|_| {
            let manifest = br#"{"icons":[{"src":"icons/48.png","sizes":"48x48"},{"src":"icons/512.png","sizes":"512x512"}]}"#;
            HashMap::from([
                (
                    "/tv",
                    ok(
                        "text/html; charset=utf-8",
                        br#"<link rel="icon" href="/fav.ico"><link rel="manifest" href="/static/app.webmanifest">"#,
                    ),
                ),
                ("/static/app.webmanifest", ok("application/manifest+json", manifest)),
                ("/static/icons/512.png", ok("image/png", PNG)),
                ("/static/icons/48.png", ok("image/png", b"\x89PNG\r\n\x1a\nsmall")),
            ])
        });
        let image = Fetcher::new(true)
            .fetch_icon(&Source::Site(server.url("/tv")))
            .unwrap();
        assert_eq!(
            image,
            Image {
                bytes: PNG.to_vec(),
                ext: "png"
            }
        );
        assert!(!server.requested().contains(&"/static/icons/48.png".into()));
    }

    #[test]
    fn falls_back_to_favicon_and_skips_bad_candidates() {
        let server = Server::start(|_| {
            HashMap::from([
                (
                    "/",
                    ok("text/html", br#"<link rel="apple-touch-icon" href="/gone.png"><link rel="icon" href="/html.png">"#),
                ),
                // Wrong content: an HTML error page served as an image.
                ("/html.png", ok("image/png", b"<html>not an image</html>")),
                ("/favicon.ico", ok("image/x-icon", b"\0\0\x01\0\x01\0data")),
            ])
        });
        let image = Fetcher::new(true)
            .fetch_icon(&Source::Site(server.url("/")))
            .unwrap();
        assert_eq!(image.ext, "ico");
    }

    #[test]
    fn icon_url_checks_type_size_and_status() {
        let big = vec![b'x'; MAX_BYTES as usize + 1];
        let mut huge_png = PNG.to_vec();
        huge_png.extend(big);
        let server = Server::start(|_| {
            HashMap::from([
                ("/ok.svg", ok("image/svg+xml", b"<svg xmlns='x'></svg>")),
                ("/text.png", ok("text/html", PNG)),
                ("/huge.png", ok("image/png", &huge_png)),
            ])
        });
        let f = Fetcher::new(true);
        let get = |p: &str| f.fetch_icon(&Source::IconUrl(server.url(p), None));
        assert_eq!(get("/ok.svg").unwrap().ext, "svg");
        assert!(get("/text.png").is_err());
        assert!(get("/huge.png").unwrap_err().to_string().contains("larger"));
        assert!(get("/missing.png").unwrap_err().to_string().contains("404"));
    }

    #[test]
    fn icon_url_with_a_hash_only_takes_that_file() {
        let server = Server::start(|_| HashMap::from([("/logo.png", ok("image/png", PNG))]));
        let f = Fetcher::new(true);
        let pinned = |hash: &str| {
            f.fetch_icon(&Source::IconUrl(
                server.url("/logo.png"),
                Some(hash.to_string()),
            ))
        };
        let right = sha256_hex(PNG);
        assert!(is_sha256_hex(&right));
        assert_eq!(pinned(&right).unwrap().bytes, PNG);
        assert_eq!(pinned(&right.to_uppercase()).unwrap().bytes, PNG);
        let wrong = "0".repeat(64);
        let err = pinned(&wrong).unwrap_err().to_string();
        assert!(err.contains("SHA-256"), "{err}");
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex(&"g".repeat(64)));
    }

    #[test]
    fn follows_redirects_but_never_to_http_or_forever() {
        let server = Server::start(|_| {
            HashMap::from([
                ("/a", (302, vec![("Location", "/b".into())], vec![])),
                ("/b", ok("image/png", PNG)),
                ("/loop", (302, vec![("Location", "/loop".into())], vec![])),
            ])
        });
        let f = Fetcher::new(true);
        assert!(
            f.fetch_icon(&Source::IconUrl(server.url("/a"), None))
                .is_ok()
        );
        assert!(
            f.fetch_icon(&Source::IconUrl(server.url("/loop"), None))
                .unwrap_err()
                .to_string()
                .contains("redirects")
        );
        // The production fetcher refuses plain http, directly and as a redirect target.
        let strict = Fetcher::new(false);
        assert!(
            strict
                .fetch_icon(&Source::IconUrl(server.url("/b"), None))
                .is_err()
        );
        assert!(
            strict
                .fetch_icon(&Source::Site("http://example.com/".into()))
                .is_err()
        );
        assert!(server.requested().iter().filter(|p| *p == "/b").count() == 1);
    }

    #[test]
    fn network_failures_are_errors_not_panics() {
        // Nothing listens on port 1.
        assert!(
            Fetcher::new(true)
                .fetch_icon(&Source::Site("http://127.0.0.1:1/".into()))
                .is_err()
        );
    }
}
