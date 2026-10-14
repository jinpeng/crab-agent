use crate::gate::{PermissionGate, Prompt};
use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use dom_smoothie::{Config as ReadabilityConfig, Readability, TextMode};
use std::time::Duration;

/// Hard cap on how much of the response body we read (defends against huge pages).
const MAX_BODY_BYTES: usize = 5_000_000;
/// Cap on the extracted text we hand back to the model.
const MAX_TEXT_BYTES: usize = 100_000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebFetchTool {
    gate: PermissionGate,
    client: reqwest::Client,
    timeout: Duration,
}

impl WebFetchTool {
    pub fn new(gate: PermissionGate) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("crab/", env!("CARGO_PKG_VERSION")))
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("reqwest client build");
        Self {
            gate,
            client,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self.client = reqwest::Client::builder()
            .user_agent(concat!("crab/", env!("CARGO_PKG_VERSION")))
            .timeout(t)
            .build()
            .expect("reqwest client build");
        self
    }

    /// Step a byte cut back to a char boundary so slicing never panics.
    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n... [truncated {} bytes]", &s[..cut], s.len() - cut)
    }
}

/// Extract the main article text from an HTML document. Returns `None` when
/// readability finds no content (so the caller can fall back to the raw body).
fn extract_article(html: &str, url: &str) -> Option<String> {
    let cfg = ReadabilityConfig {
        text_mode: TextMode::Formatted,
        ..Default::default()
    };
    let mut readability = Readability::new(html, Some(url), Some(cfg)).ok()?;
    let article = readability.parse().ok()?;
    let text = article.text_content.trim().to_string();
    if text.is_empty() {
        return None;
    }

    let mut out = String::new();
    if !article.title.trim().is_empty() {
        out.push_str("# ");
        out.push_str(article.title.trim());
        out.push('\n');
    }
    if let Some(byline) = article.byline.as_deref() {
        let byline = byline.trim();
        if !byline.is_empty() {
            out.push_str("by ");
            out.push_str(byline);
            out.push('\n');
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&text);
    Some(out)
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL over HTTP(S) and return the extracted main article text (readability), stripping navigation/ads/scripts. Long pages are truncated. Network access may prompt for confirmation."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "absolute http(s) URL to fetch" },
                "max_bytes": { "type": "integer", "description": "max extracted text bytes to return (default 100000)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'url' string".into()))?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ToolError::InvalidArgs(format!(
                "url must be absolute http(s): {url}"
            )));
        }
        let max_bytes = args["max_bytes"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(MAX_TEXT_BYTES);

        self.gate.check(Prompt {
            tool: "web_fetch",
            summary: format!("fetch: {url}"),
        })?;

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Exec(format!("fetch {url}: {e}")))?;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            return Ok(format!("[HTTP {status} for {final_url}]"));
        }

        // Read the body incrementally, capping size so a huge page can't exhaust memory.
        let mut body = Vec::new();
        let mut stream = resp;
        loop {
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    let room = MAX_BODY_BYTES.saturating_sub(body.len());
                    if room == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Ok(None) => break,
                Err(e) => return Err(ToolError::Exec(format!("read body {url}: {e}"))),
            }
        }
        let raw = String::from_utf8_lossy(&body);

        // Non-HTML (plain text, JSON, ...) is returned as-is.
        let is_html = content_type.contains("html")
            || raw.trim_start().starts_with("<!DOCTYPE")
            || raw.trim_start().starts_with("<!doctype")
            || raw.trim_start().starts_with("<html");
        let extracted = if is_html {
            extract_article(&raw, &final_url)
        } else {
            None
        };

        let text = match extracted {
            Some(t) => t,
            None => {
                let mut t = format!("[url: {final_url}]\n\n");
                t.push_str(&raw);
                t
            }
        };
        Ok(Self::truncate(&text, max_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn article_html() -> String {
        format!(
            "<!DOCTYPE html><html><head><title>The Title</title></head><body>\
             <nav>home about contact</nav>\
             <article><h1>The Title</h1>\
             <p>{}</p>\
             <p>{}</p></article>\
             <footer>copyright 2026</footer></body></html>",
            "This is the first paragraph of a sufficiently long article that readability should treat as the main content body. ".repeat(3),
            "A second paragraph continues the article to make sure extraction picks the article element over the surrounding chrome. ".repeat(3),
        )
    }

    #[tokio::test]
    async fn extracts_article_body_and_drops_chrome() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/post"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string(article_html()),
            )
            .mount(&server)
            .await;

        let t = WebFetchTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({ "url": format!("{}/post", server.uri()) }))
            .await
            .unwrap();
        assert!(
            out.contains("The Title"),
            "should keep article title: {out}"
        );
        assert!(out.contains("first paragraph"));
        assert!(!out.contains("home about contact"), "nav dropped");
        assert!(!out.contains("copyright 2026"), "footer dropped");
    }

    #[tokio::test]
    async fn non_html_returned_raw() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/data.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("plain text payload"),
            )
            .mount(&server)
            .await;

        let t = WebFetchTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({ "url": format!("{}/data.txt", server.uri()) }))
            .await
            .unwrap();
        assert!(out.contains("plain text payload"));
        assert!(out.contains("/data.txt"));
    }

    #[tokio::test]
    async fn http_error_reported_not_thrown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let t = WebFetchTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({ "url": format!("{}/missing", server.uri()) }))
            .await
            .unwrap();
        assert!(out.contains("404"), "should report status: {out}");
    }

    #[tokio::test]
    async fn denied_by_gate_makes_no_request() {
        let server = MockServer::start().await;
        let t = WebFetchTool::new(PermissionGate::auto_deny());
        let r = t
            .execute(serde_json::json!({ "url": format!("{}/post", server.uri()) }))
            .await;
        assert!(r.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_absolute_url_is_invalid_not_fetch() {
        let t = WebFetchTool::new(PermissionGate::auto_approve());
        let r = t
            .execute(serde_json::json!({ "url": "example.com/post" }))
            .await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
    }

    #[tokio::test]
    async fn long_page_is_truncated() {
        let server = MockServer::start().await;
        let html = format!(
            "<html><body><p>{}</p></body></html>",
            "word ".repeat(60_000)
        );
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(html),
            )
            .mount(&server)
            .await;

        let t = WebFetchTool::new(PermissionGate::auto_approve());
        let out = t
            .execute(serde_json::json!({ "url": server.uri(), "max_bytes": 2000 }))
            .await
            .unwrap();
        assert!(out.len() <= 2200, "should honor max_bytes");
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "中文中文中文中文";
        let out = WebFetchTool::truncate(s, 7); // 7 is not a char boundary (3-byte chars)
        assert!(out.contains("truncated"));
        // must not panic and must be valid UTF-8
        assert!(out.is_char_boundary(0));
    }
}
