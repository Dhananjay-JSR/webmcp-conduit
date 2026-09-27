//! HTTP for the page and its scripts, plus the one check that separates a
//! legitimate bridge from a scraper wearing a spec as a costume.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use url::Url;

pub struct Fetched {
    pub html: String,
    pub final_url: Url,
    /// True when the site sent `Permissions-Policy: tools=()`.
    pub tools_disabled: bool,
}

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!(
            "webmcp-conduit/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/Dhananjay-JSR/webmcp-conduit)"
        ))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("building HTTP client")
}

/// The WebMCP spec gates the API behind the `tools` policy-controlled feature,
/// and says the user agent enforces `Permissions-Policy: tools=()` *before
/// script runs*. A real browser does that for us. We are not a browser, so if
/// we injected the shim anyway we would be quietly overriding a site's
/// explicit opt-out. We detect it and refuse instead.
fn parse_tools_disabled(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all("permissions-policy")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split(',').any(|directive| {
                let d = directive.trim();
                let Some(rest) = d.strip_prefix("tools") else {
                    return false;
                };
                let rest = rest.trim_start();
                // `tools=()` — an empty allowlist disables the feature entirely.
                rest.starts_with("=()") || rest == "=()"
            })
        })
}

pub async fn page(client: &reqwest::Client, url: &Url) -> Result<Fetched> {
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("{url} returned HTTP {status}"));
    }

    let final_url = resp.url().clone();
    let tools_disabled = parse_tools_disabled(resp.headers());
    let html = resp.text().await.context("reading response body")?;

    Ok(Fetched {
        html,
        final_url,
        tools_disabled,
    })
}

/// Script CDNs a browser would load without ceremony. Module graphs routinely
/// reach these for dependencies, and blocking them does not protect anyone —
/// it just makes the page fail to register its tools.
const SCRIPT_CDNS: &[&str] = &[
    "esm.sh",
    "cdn.jsdelivr.net",
    "unpkg.com",
    "cdn.skypack.dev",
    "ga.jspm.io",
    "esm.run",
    "cdn.tailwindcss.com",
];

pub fn origin_allowed(url: &Url, page_origin: &str) -> bool {
    if url.origin().ascii_serialization() == page_origin {
        return true;
    }
    url.host_str()
        .map(|h| SCRIPT_CDNS.iter().any(|cdn| h == *cdn))
        .unwrap_or(false)
}

/// Fetch an external script. Same-origin is always allowed; beyond that only
/// well-known script CDNs are, so a page cannot pull executable code from an
/// arbitrary third party we were never asked to trust.
pub async fn script(client: &reqwest::Client, url: &Url, page_origin: &str) -> Result<String> {
    if !origin_allowed(url, page_origin) {
        return Err(anyhow!(
            "cross-origin script blocked: {url} (page origin {page_origin})"
        ));
    }
    let resp = client.get(url.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    Ok(resp.text().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("permissions-policy", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn detects_explicit_opt_out() {
        assert!(parse_tools_disabled(&headers(&["tools=()"])));
        assert!(parse_tools_disabled(&headers(&["geolocation=(), tools=()"])));
        assert!(parse_tools_disabled(&headers(&["tools=(), camera=*"])));
    }

    #[test]
    fn same_origin_and_known_cdns_are_allowed() {
        let origin = "https://app.example";
        let ok = |u: &str| origin_allowed(&Url::parse(u).unwrap(), origin);
        assert!(ok("https://app.example/main.js"));
        assert!(ok("https://esm.sh/dompurify"));
        assert!(ok("https://cdn.jsdelivr.net/npm/x"));
        assert!(!ok("https://evil.example/payload.js"));
        // A CDN name must match the host exactly, not merely be contained.
        assert!(!ok("https://esm.sh.evil.example/x.js"));
    }

    #[test]
    fn allows_sites_that_permit_tools() {
        assert!(!parse_tools_disabled(&headers(&[])));
        assert!(!parse_tools_disabled(&headers(&["tools=(self)"])));
        assert!(!parse_tools_disabled(&headers(&["tools=*"])));
        assert!(!parse_tools_disabled(&headers(&["camera=()"])));
        // Must not be fooled by a different feature that merely starts the same.
        assert!(!parse_tools_disabled(&headers(&["toolsomething=()"])));
    }
}
