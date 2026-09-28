//! A cookie jar, scoped to a session.
//!
//! Without one, `--session` only helps sites that keep their auth in
//! localStorage. Everything with a real login puts it in an HttpOnly cookie,
//! which script cannot see by design — so it has to live here, in the host,
//! alongside the requests that carry it.
//!
//! This is a deliberate subset of RFC 6265: name, value, domain, path, expiry
//! and Secure. Not implemented, and each one is a real limitation rather than
//! an oversight:
//!
//! - **SameSite** is ignored. It is a defence against cross-site requests made
//!   by a browser on a user's behalf, and there is no third-party context here
//!   to defend against: one run, one page, one session, started deliberately.
//! - **The public suffix list** is not consulted. A malicious `Set-Cookie` for
//!   `domain=.co.uk` would be accepted where a browser would refuse it. The
//!   mitigation is that a session only ever holds cookies from sites the
//!   operator pointed conduit at.
//! - **`__Host-` and `__Secure-` prefixes** are not enforced.

use crate::session::Cookie;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

#[derive(Default)]
pub struct Jar {
    /// Keyed by (domain, path, name), which is the identity RFC 6265 gives a
    /// cookie: the same name at a different path is a different cookie, and a
    /// site relies on that.
    cookies: HashMap<(String, String, String), Cookie>,
}

/// Shared with the synchronous XHR bridge inside the isolate, which runs on the
/// same thread but behind a `Function` closure the runtime owns.
pub type SharedJar = Arc<Mutex<Jar>>;

pub fn shared() -> SharedJar {
    Arc::new(Mutex::new(Jar::default()))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Jar {
    pub fn from_stored(stored: Vec<Cookie>) -> Self {
        let mut jar = Self::default();
        let now = now();
        for cookie in stored {
            // An expired cookie that was written out is simply gone, the same
            // as it would be after a browser restart.
            if cookie.expires.map(|e| e <= now).unwrap_or(false) {
                continue;
            }
            jar.insert(cookie);
        }
        jar
    }

    /// Only persistent cookies are written out. A cookie with no expiry is a
    /// session cookie, and a browser drops those when it closes — keeping them
    /// would make a session outlive what the site asked for.
    pub fn to_stored(&self) -> Vec<Cookie> {
        let now = now();
        let mut out: Vec<Cookie> = self
            .cookies
            .values()
            .filter(|c| c.expires.map(|e| e > now).unwrap_or(false))
            .cloned()
            .collect();
        out.sort_by(|a, b| (&a.domain, &a.path, &a.name).cmp(&(&b.domain, &b.path, &b.name)));
        out
    }

    fn insert(&mut self, cookie: Cookie) {
        let key = (
            cookie.domain.clone(),
            cookie.path.clone(),
            cookie.name.clone(),
        );
        self.cookies.insert(key, cookie);
    }

    /// Record a `Set-Cookie` header received from `url`.
    pub fn store(&mut self, url: &Url, header: &str) {
        let Some(cookie) = parse_set_cookie(url, header) else {
            return;
        };

        // Max-Age=0 or a date in the past is how a site deletes a cookie, most
        // importantly on sign-out. Treating it as an ordinary cookie would keep
        // a session logged in after it had been logged out.
        if cookie.expires.map(|e| e <= now()).unwrap_or(false) {
            self.cookies.remove(&(
                cookie.domain.clone(),
                cookie.path.clone(),
                cookie.name.clone(),
            ));
            return;
        }

        self.insert(cookie);
    }

    /// The `Cookie` header to send with a request to `url`, if any.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let secure_ok = url.scheme() == "https" || host == "localhost";
        let now = now();

        let mut matched: Vec<&Cookie> = self
            .cookies
            .values()
            .filter(|c| !c.expires.map(|e| e <= now).unwrap_or(false))
            .filter(|c| domain_matches(&host, &c.domain))
            .filter(|c| path_matches(path, &c.path))
            .filter(|c| !c.secure || secure_ok)
            .collect();

        if matched.is_empty() {
            return None;
        }

        // RFC 6265: longer paths first. A site that scopes a cookie to
        // /admin expects that one to win over the same name at /.
        matched.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then_with(|| a.name.cmp(&b.name))
        });

        Some(
            matched
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// RFC 6265 §5.1.3. Either an exact match, or the host is a subdomain of the
/// cookie's domain — and the boundary must be a dot, so `notexample.com` does
/// not match a cookie for `example.com`.
fn domain_matches(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// RFC 6265 §5.1.4.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

/// The default path is the request's directory, not "/" — a subtlety that
/// decides whether a cookie set at /app/login comes back at /app/data.
fn default_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

fn parse_set_cookie(url: &Url, header: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let host = url.host_str()?.to_ascii_lowercase();
    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain: host.clone(),
        path: default_path(url),
        expires: None,
        secure: false,
    };

    let mut max_age: Option<i64> = None;
    let mut expires: Option<i64> = None;

    for attr in parts {
        let attr = attr.trim();
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (attr.to_ascii_lowercase(), String::new()),
        };

        match key.as_str() {
            "domain" => {
                let requested = val.trim_start_matches('.').to_ascii_lowercase();
                if requested.is_empty() {
                    continue;
                }
                // A site may only widen a cookie to a domain it belongs to.
                // Without this check any site could set a cookie for any other.
                if domain_matches(&host, &requested) {
                    cookie.domain = requested;
                }
            }
            "path" => {
                if val.starts_with('/') {
                    cookie.path = val;
                }
            }
            "secure" => cookie.secure = true,
            "max-age" => max_age = val.parse::<i64>().ok().map(|s| now() + s),
            "expires" => expires = parse_http_date(&val),
            _ => {}
        }
    }

    // Max-Age wins over Expires where both are present, per RFC 6265 §5.3.
    cookie.expires = max_age.or(expires);
    Some(cookie)
}

/// IMF-fixdate, as in `Wed, 21 Oct 2015 07:28:00 GMT`. The obsolete formats
/// RFC 6265 also permits are not handled; a cookie whose date we cannot read
/// becomes a session cookie, which errs toward forgetting rather than toward
/// keeping something too long.
fn parse_http_date(value: &str) -> Option<i64> {
    let cleaned = value.trim().trim_end_matches(" GMT");
    let rest = cleaned.split_once(", ").map(|(_, r)| r).unwrap_or(cleaned);
    let mut fields = rest.split_whitespace();

    let day: i64 = fields.next()?.parse().ok()?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = fields.next()?.parse().ok()?;

    let time = fields.next()?;
    let mut hms = time.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let s: i64 = hms.next()?.parse().ok()?;

    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + s)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn a_cookie_comes_back_to_the_site_that_set_it() {
        let mut jar = Jar::default();
        jar.store(&url("https://example.com/"), "sid=abc; Path=/");
        assert_eq!(
            jar.header_for(&url("https://example.com/anything")),
            Some("sid=abc".into())
        );
        // ...and nowhere else.
        assert_eq!(jar.header_for(&url("https://other.example/")), None);
    }

    #[test]
    fn a_site_cannot_set_a_cookie_for_someone_else() {
        let mut jar = Jar::default();
        jar.store(&url("https://evil.example/"), "sid=abc; Domain=example.com");
        assert_eq!(jar.header_for(&url("https://example.com/")), None);
    }

    #[test]
    fn subdomains_match_only_on_a_dot_boundary() {
        assert!(domain_matches("app.example.com", "example.com"));
        assert!(domain_matches("example.com", "example.com"));
        // The case that makes a naive `ends_with` a security bug.
        assert!(!domain_matches("notexample.com", "example.com"));
        assert!(!domain_matches("example.com", "app.example.com"));
    }

    #[test]
    fn paths_match_on_segment_boundaries() {
        assert!(path_matches("/app/data", "/app"));
        assert!(path_matches("/app", "/app"));
        assert!(path_matches("/app/data", "/"));
        // /application is not inside /app.
        assert!(!path_matches("/application", "/app"));
    }

    #[test]
    fn the_default_path_is_the_directory_not_the_root() {
        assert_eq!(default_path(&url("https://e.com/app/login")), "/app");
        assert_eq!(default_path(&url("https://e.com/login")), "/");
        assert_eq!(default_path(&url("https://e.com/")), "/");
    }

    #[test]
    fn secure_cookies_stay_off_plain_http() {
        let mut jar = Jar::default();
        jar.store(&url("https://example.com/"), "sid=abc; Secure");
        assert!(jar.header_for(&url("https://example.com/")).is_some());
        assert_eq!(jar.header_for(&url("http://example.com/")), None);
    }

    #[test]
    fn signing_out_actually_removes_the_cookie() {
        let mut jar = Jar::default();
        jar.store(&url("https://example.com/"), "sid=abc; Max-Age=3600");
        assert!(jar.header_for(&url("https://example.com/")).is_some());

        // What a sign-out endpoint sends.
        jar.store(&url("https://example.com/"), "sid=; Max-Age=0");
        assert_eq!(jar.header_for(&url("https://example.com/")), None);
    }

    #[test]
    fn only_persistent_cookies_survive_the_run() {
        let mut jar = Jar::default();
        jar.store(&url("https://example.com/"), "keep=1; Max-Age=3600");
        jar.store(&url("https://example.com/"), "drop=1");

        let stored = jar.to_stored();
        assert_eq!(stored.len(), 1, "session cookies should not persist");
        assert_eq!(stored[0].name, "keep");

        // And they come back on the next run.
        let reloaded = Jar::from_stored(stored);
        assert_eq!(
            reloaded.header_for(&url("https://example.com/")),
            Some("keep=1".into())
        );
    }

    #[test]
    fn longer_paths_are_sent_first() {
        let mut jar = Jar::default();
        jar.store(&url("https://example.com/"), "a=root; Path=/");
        jar.store(&url("https://example.com/"), "a=deep; Path=/admin");
        assert_eq!(
            jar.header_for(&url("https://example.com/admin/x")),
            Some("a=deep; a=root".into())
        );
    }

    #[test]
    fn http_dates_parse() {
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(parse_http_date("nonsense"), None);
    }

    #[test]
    fn max_age_beats_expires() {
        let mut jar = Jar::default();
        jar.store(
            &url("https://example.com/"),
            "sid=abc; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Max-Age=3600",
        );
        // Expired by Expires, alive by Max-Age. Max-Age wins, so it is kept.
        assert!(jar.header_for(&url("https://example.com/")).is_some());
    }
}
