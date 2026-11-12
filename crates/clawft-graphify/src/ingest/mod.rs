//! Ingestion sources for graphify knowledge graphs.
//!
//! - URL ingestion: fetch URLs (tweets, arXiv, PDFs, webpages) and save as
//!   annotated markdown ready for extraction into the knowledge graph
//!   (ported from Python `graphify/ingest.py`). Security: blocks private IPs
//!   and `file://` schemes to prevent SSRF.
//! - OWL/RDF ingestion ([`rdf`], feature `rdf-ingest`): Turtle and JSON-LD →
//!   entities + relationships (WEFT-358 / OG-2).

#[cfg(feature = "rdf-ingest")]
pub mod rdf;

#[cfg(feature = "rdf-ingest")]
pub use rdf::{
    ONTOLOGY_DOMAIN, RdfIngestFormat, RdfIngestOptions, RdfIngestStats, local_name, parse_json_ld,
    parse_rdf, parse_rdf_file, parse_rdf_str, parse_turtle,
};

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::GraphifyError;

// ---------------------------------------------------------------------------
// URL type detection
// ---------------------------------------------------------------------------

/// Classified URL type for targeted extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlType {
    Tweet,
    Arxiv,
    Github,
    Youtube,
    Pdf,
    Image,
    Webpage,
}

/// Classify a URL for targeted extraction.
pub fn detect_url_type(url: &str) -> UrlType {
    let lower = url.to_lowercase();
    if lower.contains("twitter.com") || lower.contains("x.com") {
        return UrlType::Tweet;
    }
    if lower.contains("arxiv.org") {
        return UrlType::Arxiv;
    }
    if lower.contains("github.com") {
        return UrlType::Github;
    }
    if lower.contains("youtube.com") || lower.contains("youtu.be") {
        return UrlType::Youtube;
    }
    if let Some(path) = url.split('?').next() {
        let path_lower = path.to_lowercase();
        if path_lower.ends_with(".pdf") {
            return UrlType::Pdf;
        }
        for ext in &[".png", ".jpg", ".jpeg", ".webp", ".gif"] {
            if path_lower.ends_with(ext) {
                return UrlType::Image;
            }
        }
    }
    UrlType::Webpage
}

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

/// Validate that a URL is safe to fetch (no SSRF).
pub fn validate_url(url: &str) -> Result<(), GraphifyError> {
    let lower = url.to_lowercase();

    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(GraphifyError::IngestError(format!(
            "only http:// and https:// URLs are allowed, got: {url}"
        )));
    }

    let after_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" {
        return Err(GraphifyError::IngestError(
            "cannot fetch localhost URLs (SSRF protection)".into(),
        ));
    }

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let octets = ip.octets();
        let is_private = octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168)
            || octets[0] == 127;
        if is_private {
            return Err(GraphifyError::IngestError(
                "cannot fetch private IP addresses (SSRF protection)".into(),
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Filename helpers
// ---------------------------------------------------------------------------

/// Turn a URL into a safe filename.
pub fn safe_filename(url: &str, suffix: &str) -> String {
    let re = Regex::new(r"[^\w\-]").unwrap();
    let multi_underscore = Regex::new(r"_+").unwrap();

    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    let name = re.replace_all(after_scheme, "_");
    let name = multi_underscore.replace_all(&name, "_");
    let name = name.trim_matches('_');

    let truncated = if name.len() > 80 { &name[..80] } else { name };
    format!("{truncated}{suffix}")
}

/// Escape a string for embedding in YAML double-quoted scalar.
fn yaml_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

// ---------------------------------------------------------------------------
// IngestResult
// ---------------------------------------------------------------------------

/// Result of URL ingestion.
#[derive(Debug)]
pub struct IngestResult {
    /// Path to the saved file.
    pub path: PathBuf,
    /// The detected URL type.
    pub url_type: UrlType,
    /// Filename that was saved.
    pub filename: String,
}

// ---------------------------------------------------------------------------
// HTTP client trait
// ---------------------------------------------------------------------------

/// Abstraction over HTTP fetching so callers can inject their own client.
pub trait HttpClient: Send + Sync {
    /// Fetch a URL and return the body as a string.
    fn fetch_text(&self, url: &str) -> Result<String, GraphifyError>;
    /// Fetch a URL and return raw bytes.
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, GraphifyError>;
}

/// A no-op HTTP client that always errors (for compile-time gating / tests).
///
/// Production binaries should use [`ReqwestHttpClient`] (feature `http-client`).
pub struct StubHttpClient;

impl HttpClient for StubHttpClient {
    fn fetch_text(&self, url: &str) -> Result<String, GraphifyError> {
        Err(GraphifyError::IngestError(format!(
            "HTTP client not configured, cannot fetch: {url}"
        )))
    }
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, GraphifyError> {
        Err(GraphifyError::IngestError(format!(
            "HTTP client not configured, cannot fetch: {url}"
        )))
    }
}

/// Production HTTP client backed by `reqwest` (blocking + rustls).
///
/// Enabled with the `http-client` feature. SSRF checks run in [`ingest`]
/// before any fetch; this client also re-validates URLs as defense in depth
/// when used directly.
#[cfg(feature = "http-client")]
pub struct ReqwestHttpClient {
    client: reqwest::blocking::Client,
}

#[cfg(feature = "http-client")]
impl ReqwestHttpClient {
    /// Build a client with sensible timeouts and a graphify user-agent.
    pub fn new() -> Result<Self, GraphifyError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(concat!("weftos-graphify/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| GraphifyError::IngestError(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { client })
    }

    fn map_status(url: &str, status: reqwest::StatusCode) -> Result<(), GraphifyError> {
        if status.is_success() {
            Ok(())
        } else {
            Err(GraphifyError::IngestError(format!(
                "HTTP {status} fetching {url}"
            )))
        }
    }
}

#[cfg(feature = "http-client")]
impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new().expect("failed to build default ReqwestHttpClient")
    }
}

#[cfg(feature = "http-client")]
impl HttpClient for ReqwestHttpClient {
    fn fetch_text(&self, url: &str) -> Result<String, GraphifyError> {
        // Defense in depth: same SSRF policy as ingest(), for direct callers.
        validate_url(url)?;
        let resp = self.client.get(url).send().map_err(|e| {
            GraphifyError::IngestError(format!("HTTP request failed for {url}: {e}"))
        })?;
        Self::map_status(url, resp.status())?;
        resp.text().map_err(|e| {
            GraphifyError::IngestError(format!("failed to read response body from {url}: {e}"))
        })
    }

    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, GraphifyError> {
        validate_url(url)?;
        let resp = self.client.get(url).send().map_err(|e| {
            GraphifyError::IngestError(format!("HTTP request failed for {url}: {e}"))
        })?;
        Self::map_status(url, resp.status())?;
        resp.bytes().map(|b| b.to_vec()).map_err(|e| {
            GraphifyError::IngestError(format!("failed to read response bytes from {url}: {e}"))
        })
    }
}

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

fn strip_html(html: &str) -> String {
    let script_re = Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let style_re = Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let tag_re = Regex::new(r"<[^>]+>").unwrap();
    let ws_re = Regex::new(r"\s+").unwrap();

    let text = script_re.replace_all(html, "");
    let text = style_re.replace_all(&text, "");
    let text = tag_re.replace_all(&text, " ");
    let text = ws_re.replace_all(&text, " ");
    let text = text.trim().to_string();

    if text.len() > 12_000 {
        text[..12_000].to_string()
    } else {
        text
    }
}

fn extract_title(html: &str) -> Option<String> {
    let re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
    re.captures(html).map(|c| {
        let ws_re = Regex::new(r"\s+").unwrap();
        ws_re
            .replace_all(c.get(1).unwrap().as_str(), " ")
            .trim()
            .to_string()
    })
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

fn fetch_tweet(
    client: &dyn HttpClient,
    url: &str,
    contributor: Option<&str>,
) -> Result<(String, String), GraphifyError> {
    let oembed_url = url.replace("x.com", "twitter.com");
    let api_url = format!(
        "https://publish.twitter.com/oembed?url={}&omit_script=true",
        urlencoding_encode(&oembed_url)
    );

    let (tweet_text, tweet_author) = match client.fetch_text(&api_url) {
        Ok(body) => {
            let data: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
            let html = data["html"].as_str().unwrap_or("");
            let tag_re = Regex::new(r"<[^>]+>").unwrap();
            let text = tag_re.replace_all(html, "").trim().to_string();
            let author = data["author_name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            (text, author)
        }
        Err(_) => (
            format!("Tweet at {url} (could not fetch content)"),
            "unknown".to_string(),
        ),
    };

    let now = chrono_now_iso();
    let content = format!(
        "---\nsource_url: {url}\ntype: tweet\nauthor: {tweet_author}\ncaptured_at: {now}\ncontributor: {cont}\n---\n\n# Tweet by @{tweet_author}\n\n{tweet_text}\n\nSource: {url}\n",
        url = url,
        tweet_author = tweet_author,
        now = now,
        cont = contributor.unwrap_or("unknown"),
        tweet_text = tweet_text,
    );
    let filename = safe_filename(url, ".md");
    Ok((content, filename))
}

fn fetch_arxiv(
    client: &dyn HttpClient,
    url: &str,
    contributor: Option<&str>,
) -> Result<(String, String), GraphifyError> {
    let arxiv_re = Regex::new(r"(\d{4}\.\d{4,5})").unwrap();
    let arxiv_id = match arxiv_re.captures(url) {
        Some(caps) => caps.get(1).unwrap().as_str().to_string(),
        None => return fetch_webpage(client, url, contributor),
    };

    let api_url = format!("https://export.arxiv.org/abs/{arxiv_id}");
    let (title, abstract_text, authors) = match client.fetch_text(&api_url) {
        Ok(html) => {
            let tag_re = Regex::new(r"<[^>]+>").unwrap();
            let abs_re =
                Regex::new(r#"(?is)class="abstract[^"]*"[^>]*>(.*?)</blockquote>"#).unwrap();
            let title_re = Regex::new(r#"(?is)class="title[^"]*"[^>]*>(.*?)</h1>"#).unwrap();
            let auth_re = Regex::new(r#"(?is)class="authors"[^>]*>(.*?)</div>"#).unwrap();

            let abstract_text = abs_re
                .captures(&html)
                .map(|c| {
                    tag_re
                        .replace_all(c.get(1).unwrap().as_str(), "")
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            let title = title_re
                .captures(&html)
                .map(|c| {
                    tag_re
                        .replace_all(c.get(1).unwrap().as_str(), " ")
                        .trim()
                        .to_string()
                })
                .unwrap_or_else(|| arxiv_id.clone());
            let authors = auth_re
                .captures(&html)
                .map(|c| {
                    tag_re
                        .replace_all(c.get(1).unwrap().as_str(), "")
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            (title, abstract_text, authors)
        }
        Err(_) => (arxiv_id.clone(), String::new(), String::new()),
    };

    let now = chrono_now_iso();
    let content = format!(
        "---\nsource_url: {url}\narxiv_id: {aid}\ntype: paper\ntitle: \"{t}\"\npaper_authors: \"{a}\"\ncaptured_at: {now}\ncontributor: {cont}\n---\n\n# {title}\n\n**Authors:** {authors}\n**arXiv:** {aid}\n\n## Abstract\n\n{abs}\n\nSource: {url}\n",
        url = url,
        aid = arxiv_id,
        t = yaml_escape(&title),
        a = yaml_escape(&authors),
        now = now,
        cont = contributor.unwrap_or("unknown"),
        title = title,
        authors = authors,
        abs = abstract_text,
    );
    let filename = format!("arxiv_{}.md", arxiv_id.replace('.', "_"));
    Ok((content, filename))
}

fn fetch_webpage(
    client: &dyn HttpClient,
    url: &str,
    contributor: Option<&str>,
) -> Result<(String, String), GraphifyError> {
    let html = client.fetch_text(url)?;
    let title = extract_title(&html).unwrap_or_else(|| url.to_string());
    let markdown = strip_html(&html);

    let now = chrono_now_iso();
    let content = format!(
        "---\nsource_url: {url}\ntype: webpage\ntitle: \"{t}\"\ncaptured_at: {now}\ncontributor: {cont}\n---\n\n# {title}\n\nSource: {url}\n\n---\n\n{md}\n",
        url = url,
        t = yaml_escape(&title),
        now = now,
        cont = contributor.unwrap_or("unknown"),
        title = title,
        md = markdown,
    );
    let filename = safe_filename(url, ".md");
    Ok((content, filename))
}

// ---------------------------------------------------------------------------
// Core ingestion
// ---------------------------------------------------------------------------

/// Chain event kind for URL ingestion.
pub const EVENT_KIND_GRAPHIFY_INGEST: &str = "graphify.ingest";

/// Chain event kind for saved Q&A query results (WEFT-153).
///
/// Emitted by [`save_query_result`]. Mirrors
/// `clawft_kernel::chain::EVENT_KIND_GRAPHIFY_QUERY_RESULT` and
/// `clawft_core::chain_event::EVENT_KIND_GRAPHIFY_QUERY_RESULT`.
pub const EVENT_KIND_GRAPHIFY_QUERY_RESULT: &str = "graphify.query_result";

/// Ingest a URL: fetch, classify, and save to `target_dir`.
pub fn ingest(
    url: &str,
    target_dir: &Path,
    client: &dyn HttpClient,
    contributor: Option<&str>,
) -> Result<IngestResult, GraphifyError> {
    validate_url(url)?;

    std::fs::create_dir_all(target_dir)
        .map_err(|e| GraphifyError::IngestError(format!("failed to create target dir: {e}")))?;

    let url_type = detect_url_type(url);

    let result = match url_type {
        UrlType::Pdf => {
            let bytes = client.fetch_bytes(url)?;
            let filename = safe_filename(url, ".pdf");
            let out_path = target_dir.join(&filename);
            std::fs::write(&out_path, bytes)?;
            IngestResult {
                path: out_path,
                url_type,
                filename,
            }
        }
        UrlType::Image => {
            let ext = url
                .rsplit('.')
                .next()
                .map(|e| format!(".{}", e.split('?').next().unwrap_or("jpg")))
                .unwrap_or_else(|| ".jpg".to_string());
            let bytes = client.fetch_bytes(url)?;
            let filename = safe_filename(url, &ext);
            let out_path = target_dir.join(&filename);
            std::fs::write(&out_path, bytes)?;
            IngestResult {
                path: out_path,
                url_type,
                filename,
            }
        }
        _ => {
            let (content, filename) = match url_type {
                UrlType::Tweet => fetch_tweet(client, url, contributor)?,
                UrlType::Arxiv => fetch_arxiv(client, url, contributor)?,
                _ => fetch_webpage(client, url, contributor)?,
            };

            let mut out_path = target_dir.join(&filename);
            let mut counter = 1u32;
            while out_path.exists() {
                let stem = Path::new(&filename)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file");
                out_path = target_dir.join(format!("{stem}_{counter}.md"));
                counter += 1;
            }

            std::fs::write(&out_path, &content)?;
            let final_filename = out_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&filename)
                .to_string();

            IngestResult {
                path: out_path,
                url_type,
                filename: final_filename,
            }
        }
    };

    // Chain event marker -- daemon subscriber forwards to ExoChain.
    tracing::info!(
        target: "chain_event",
        source = "graphify",
        kind = EVENT_KIND_GRAPHIFY_INGEST,
        url = url,
        url_type = ?result.url_type,
        filename = %result.filename,
        "chain"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Query result storage (feedback loop)
// ---------------------------------------------------------------------------

/// Save a Q&A result as markdown for re-extraction into the graph.
pub fn save_query_result(
    question: &str,
    answer: &str,
    memory_dir: &Path,
    query_type: &str,
    source_nodes: Option<&[String]>,
) -> Result<PathBuf, GraphifyError> {
    std::fs::create_dir_all(memory_dir)?;

    let now = chrono_now_iso();
    let slug_re = Regex::new(r"[^\w]").unwrap();
    let lowered = question.to_lowercase();
    let slug = slug_re.replace_all(&lowered, "_");
    let slug = if slug.len() > 50 { &slug[..50] } else { &slug };
    let slug = slug.trim_matches('_');

    let ts = now
        .replace([':', '-', 'T'], "")
        .split('.')
        .next()
        .unwrap_or("0")
        .to_string();
    let filename = format!("query_{ts}_{slug}.md");

    let mut lines = vec![
        "---".to_string(),
        format!("type: \"{query_type}\""),
        format!("date: \"{now}\""),
        format!("question: \"{}\"", yaml_escape(question)),
        "contributor: \"graphify\"".to_string(),
    ];

    if let Some(nodes) = source_nodes {
        let nodes_str: Vec<String> = nodes.iter().take(10).map(|n| format!("\"{n}\"")).collect();
        lines.push(format!("source_nodes: [{}]", nodes_str.join(", ")));
    }

    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(format!("# Q: {question}"));
    lines.push(String::new());
    lines.push("## Answer".to_string());
    lines.push(String::new());
    lines.push(answer.to_string());

    if let Some(nodes) = source_nodes {
        lines.push(String::new());
        lines.push("## Source Nodes".to_string());
        lines.push(String::new());
        for n in nodes {
            lines.push(format!("- {n}"));
        }
    }

    let content = lines.join("\n");
    let out_path = memory_dir.join(&filename);
    std::fs::write(&out_path, &content)?;

    // Chain event marker — daemon subscriber forwards to ExoChain (WEFT-153).
    let source_node_count = source_nodes.map(|n| n.len()).unwrap_or(0);
    tracing::info!(
        target: "chain_event",
        source = "graphify",
        kind = EVENT_KIND_GRAPHIFY_QUERY_RESULT,
        query_type = query_type,
        path = %out_path.display(),
        source_node_count = source_node_count,
        "chain"
    );

    Ok(out_path)
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

fn urlencoding_encode(s: &str) -> String {
    s.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('&', "%26")
        .replace('?', "%3F")
}

fn chrono_now_iso() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    format!("1970-01-01T00:00:00Z+{secs}s")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_type_detection() {
        assert_eq!(
            detect_url_type("https://twitter.com/user/status/123"),
            UrlType::Tweet
        );
        assert_eq!(
            detect_url_type("https://x.com/user/status/456"),
            UrlType::Tweet
        );
        assert_eq!(
            detect_url_type("https://arxiv.org/abs/2301.12345"),
            UrlType::Arxiv
        );
        assert_eq!(
            detect_url_type("https://github.com/user/repo"),
            UrlType::Github
        );
        assert_eq!(detect_url_type("https://example.com/doc.pdf"), UrlType::Pdf);
        assert_eq!(
            detect_url_type("https://example.com/img.png"),
            UrlType::Image
        );
        assert_eq!(
            detect_url_type("https://example.com/page"),
            UrlType::Webpage
        );
    }

    #[test]
    fn ssrf_protection() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("http://127.0.0.1/admin").is_err());
        assert!(validate_url("http://localhost/api").is_err());
        assert!(validate_url("http://10.0.0.1/internal").is_err());
        assert!(validate_url("http://192.168.1.1/router").is_err());
        assert!(validate_url("http://172.16.0.1/private").is_err());
        assert!(validate_url("https://example.com/page").is_ok());
    }

    #[test]
    fn safe_filename_generation() {
        let name = safe_filename("https://example.com/path/to/page", ".md");
        assert!(name.ends_with(".md"));
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
    }

    #[test]
    fn strip_html_basic() {
        let html = "<html><body><p>Hello <b>world</b></p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn save_query_result_creates_file() {
        let dir = std::env::temp_dir().join("graphify_test_query");
        let _ = std::fs::remove_dir_all(&dir);

        let path = save_query_result(
            "What is the main service?",
            "The AuthService handles authentication.",
            &dir,
            "query",
            Some(&["AuthService".to_string()]),
        )
        .unwrap();

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("AuthService"));
        assert!(content.contains("question:"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WEFT-153: save_query_result emits graphify.query_result chain kind.
    #[test]
    fn save_query_result_emits_chain_event_kind() {
        // Constant must stay aligned with clawft-kernel / clawft-core.
        assert_eq!(EVENT_KIND_GRAPHIFY_QUERY_RESULT, "graphify.query_result");

        let dir = std::env::temp_dir().join("graphify_test_query_chain");
        let _ = std::fs::remove_dir_all(&dir);

        // Smoke: write succeeds (chain emission is via tracing target;
        // full bridge coverage is in clawft-weave). Kind constant is the
        // contract under test here.
        let path = save_query_result(
            "What emits the chain event?",
            "save_query_result does.",
            &dir,
            "query",
            Some(&["GraphifyIngest".to_string()]),
        )
        .unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yaml_escape_special_chars() {
        assert_eq!(yaml_escape("hello \"world\""), "hello \\\"world\\\"");
        assert_eq!(yaml_escape("line\nbreak"), "line break");
    }

    /// Hermetic mock: returns canned bodies for known URLs.
    struct MockHttpClient {
        text: std::collections::HashMap<String, String>,
        bytes: std::collections::HashMap<String, Vec<u8>>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                text: std::collections::HashMap::new(),
                bytes: std::collections::HashMap::new(),
            }
        }

        fn with_text(mut self, url: &str, body: &str) -> Self {
            self.text.insert(url.to_string(), body.to_string());
            self
        }

        fn with_bytes(mut self, url: &str, body: Vec<u8>) -> Self {
            self.bytes.insert(url.to_string(), body);
            self
        }
    }

    impl HttpClient for MockHttpClient {
        fn fetch_text(&self, url: &str) -> Result<String, GraphifyError> {
            self.text
                .get(url)
                .cloned()
                .ok_or_else(|| GraphifyError::IngestError(format!("mock: no text for {url}")))
        }

        fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, GraphifyError> {
            self.bytes
                .get(url)
                .cloned()
                .ok_or_else(|| GraphifyError::IngestError(format!("mock: no bytes for {url}")))
        }
    }

    #[test]
    fn stub_client_errors() {
        let client = StubHttpClient;
        assert!(client.fetch_text("https://example.com").is_err());
        assert!(client.fetch_bytes("https://example.com/a.pdf").is_err());
    }

    #[test]
    fn ingest_webpage_with_mock_client() {
        let url = "https://example.com/docs/intro";
        let html = r#"<html><head><title>Intro Guide</title></head>
<body><script>bad()</script><p>Hello knowledge graph.</p></body></html>"#;
        let client = MockHttpClient::new().with_text(url, html);

        let dir = std::env::temp_dir().join("graphify_test_ingest_webpage");
        let _ = std::fs::remove_dir_all(&dir);

        let result = ingest(url, &dir, &client, Some("tester")).unwrap();
        assert_eq!(result.url_type, UrlType::Webpage);
        assert!(result.path.exists());
        let content = std::fs::read_to_string(&result.path).unwrap();
        assert!(content.contains("Intro Guide"));
        assert!(content.contains("Hello knowledge graph"));
        assert!(content.contains("source_url: https://example.com/docs/intro"));
        assert!(content.contains("contributor: tester"));
        assert!(!content.contains("<script"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_arxiv_with_mock_client() {
        let url = "https://arxiv.org/abs/2301.12345";
        let abs_url = "https://export.arxiv.org/abs/2301.12345";
        let html = r#"
            <h1 class="title mathjax"><span class="descriptor">Title:</span>Cool Paper</h1>
            <div class="authors"><a>Ada Lovelace</a></div>
            <blockquote class="abstract mathjax">
                <span class="descriptor">Abstract:</span> We prove things.
            </blockquote>
        "#;
        let client = MockHttpClient::new().with_text(abs_url, html);

        let dir = std::env::temp_dir().join("graphify_test_ingest_arxiv");
        let _ = std::fs::remove_dir_all(&dir);

        let result = ingest(url, &dir, &client, None).unwrap();
        assert_eq!(result.url_type, UrlType::Arxiv);
        assert_eq!(result.filename, "arxiv_2301_12345.md");
        let content = std::fs::read_to_string(&result.path).unwrap();
        assert!(content.contains("Cool Paper"));
        assert!(content.contains("Ada Lovelace"));
        assert!(content.contains("We prove things"));
        assert!(content.contains("arxiv_id: 2301.12345"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_pdf_with_mock_client() {
        let url = "https://example.com/papers/report.pdf";
        let client = MockHttpClient::new().with_bytes(url, b"%PDF-1.4 mock".to_vec());

        let dir = std::env::temp_dir().join("graphify_test_ingest_pdf");
        let _ = std::fs::remove_dir_all(&dir);

        let result = ingest(url, &dir, &client, None).unwrap();
        assert_eq!(result.url_type, UrlType::Pdf);
        assert!(result.filename.ends_with(".pdf"));
        let bytes = std::fs::read(&result.path).unwrap();
        assert_eq!(bytes, b"%PDF-1.4 mock");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_rejects_ssrf_before_fetch() {
        let client = MockHttpClient::new();
        let dir = std::env::temp_dir().join("graphify_test_ssrf_ingest");
        let err = ingest("http://127.0.0.1/secret", &dir, &client, None).unwrap_err();
        assert!(err.to_string().contains("SSRF") || err.to_string().contains("localhost"));
    }
}

// ---------------------------------------------------------------------------
// ReqwestHttpClient tests (feature-gated, hermetic via mockito)
// ---------------------------------------------------------------------------
//
// mockito binds localhost; `validate_url` / `ReqwestHttpClient` block private
// IPs. Transport tests use a thin blocking client that skips SSRF so we can
// prove reqwest fetch works end-to-end. Full ingest SSRF coverage is in the
// mock suite above. Tests are sync because `reqwest::blocking` owns its own
// runtime and must not be created/dropped inside a tokio async test.

#[cfg(all(test, feature = "http-client"))]
mod reqwest_tests {
    use super::*;

    /// Thin wrapper that hits mockito without SSRF (localhost).
    struct LocalHttpClient {
        inner: reqwest::blocking::Client,
    }

    impl LocalHttpClient {
        fn new() -> Self {
            Self {
                inner: reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap(),
            }
        }
    }

    impl HttpClient for LocalHttpClient {
        fn fetch_text(&self, url: &str) -> Result<String, GraphifyError> {
            let resp = self
                .inner
                .get(url)
                .send()
                .map_err(|e| GraphifyError::IngestError(format!("local fetch_text: {e}")))?;
            if !resp.status().is_success() {
                return Err(GraphifyError::IngestError(format!(
                    "HTTP {} from local mock",
                    resp.status()
                )));
            }
            resp.text()
                .map_err(|e| GraphifyError::IngestError(e.to_string()))
        }

        fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, GraphifyError> {
            let resp = self
                .inner
                .get(url)
                .send()
                .map_err(|e| GraphifyError::IngestError(format!("local fetch_bytes: {e}")))?;
            if !resp.status().is_success() {
                return Err(GraphifyError::IngestError(format!(
                    "HTTP {} from local mock",
                    resp.status()
                )));
            }
            resp.bytes()
                .map(|b| b.to_vec())
                .map_err(|e| GraphifyError::IngestError(e.to_string()))
        }
    }

    #[test]
    fn reqwest_blocking_client_builds_and_enforces_ssrf() {
        let client = ReqwestHttpClient::new().expect("build client");
        assert!(client.fetch_text("http://127.0.0.1/").is_err());
        assert!(client.fetch_text("http://10.0.0.1/").is_err());
        assert!(client.fetch_text("http://localhost/admin").is_err());
        assert!(client.fetch_bytes("file:///etc/passwd").is_err());
    }

    #[test]
    fn hermetic_fetch_text_and_bytes_via_mockito() {
        let mut server = mockito::Server::new();
        let _page = server
            .mock("GET", "/page")
            .with_status(200)
            .with_body("<html><title>Wire</title><body>ok</body></html>")
            .create();
        let _bin = server
            .mock("GET", "/file.bin")
            .with_status(200)
            .with_body(vec![1u8, 2, 3, 4])
            .create();

        let client = LocalHttpClient::new();
        let text = client
            .fetch_text(&format!("{}/page", server.url()))
            .unwrap();
        assert!(text.contains("Wire"));

        let bytes = client
            .fetch_bytes(&format!("{}/file.bin", server.url()))
            .unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn hermetic_http_error_status() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/missing")
            .with_status(404)
            .with_body("nope")
            .create();

        let err = LocalHttpClient::new()
            .fetch_text(&format!("{}/missing", server.url()))
            .unwrap_err();
        assert!(err.to_string().contains("404") || err.to_string().contains("HTTP"));
    }
}
