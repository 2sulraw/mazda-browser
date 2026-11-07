//! Cookie jar (RFC 6265 subset) + disk persistence.
//!
//! The engine sent every request anonymously: a response's `Set-Cookie` was
//! thrown away and `document.cookie` did not exist. Static pages don't care,
//! but anything with a session does — a login POST's cookie never came back
//! on the redirect, so the page stayed logged out.
//!
//! What is modeled: name/value, `Domain` (host-only vs suffix match), `Path`,
//! `Max-Age`/`Expires`, `Secure` and `HttpOnly`. The jar sends matching
//! cookies on every GET/POST (page, stylesheet, script, image, font) and
//! persists to `%LOCALAPPDATA%\mazda-cache\cookies` so a session survives a
//! restart. The file is capped (oldest/expired evicted) and written at most
//! once per second.
//!
//! shortcuts (honest, none affects static rendering):
//! - `SameSite` is parsed away: every request this engine makes is a
//!   same-site, user-initiated navigation or subresource, so there is no
//!   cross-site context to restrict.
//! - `Expires` accepts the RFC 1123 form (`Wed, 21 Oct 2015 07:28:00 GMT`);
//!   any other date syntax is ignored and the cookie stays a session cookie.
//! - No public-suffix list: a `Domain` attribute must still be a suffix of
//!   the request host, so a page can only widen its cookie to its own
//!   subdomains — never to a sibling or a TLD.
//! - Cookies set on an HTTP redirect hop are lost (the client follows
//!   redirects internally and only the final response is exposed).
//! - There is no per-site partition: the jar is one process-wide store, like
//!   a browser profile with a single context.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Global cookie cap. 3000 x ~100 bytes ≈ 300 KB of RAM — small enough to
/// ignore against the page budget, large enough for any real session.
const MAX_COOKIES: usize = 3000;
const FILE_HEADER: &str = "MCOOKIE1";

/// One stored cookie.
#[derive(Clone, Debug, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// lower-case, no leading dot
    pub domain: String,
    /// true = the request host must equal `domain` exactly (no `Domain` attr)
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// unix seconds; `None` = session cookie (kept until the process ends)
    pub expires: Option<i64>,
    pub created: i64,
}

impl Cookie {
    /// Identity of a cookie: (name, domain, path). A new value replaces it.
    fn same_key(&self, name: &str, domain: &str, path: &str) -> bool {
        self.name == name && self.domain == domain && self.path == path
    }
}

/// In-memory jar. Pure (no I/O, no globals) so it is unit-testable.
pub struct Jar {
    pub cookies: Vec<Cookie>,
    pub cap: usize,
}

impl Default for Jar {
    fn default() -> Self {
        Jar::new()
    }
}

impl Jar {
    pub fn new() -> Jar {
        Jar {
            cookies: Vec::new(),
            cap: MAX_COOKIES,
        }
    }

    /// Store one `Set-Cookie` value received from `url`. Returns true when
    /// the jar changed (a new/replaced cookie, or a deletion).
    pub fn set_cookie(&mut self, url: &str, line: &str, now: i64) -> bool {
        let Some(u) = UrlParts::parse(url) else {
            return false;
        };
        let mut attrs = line.split(';');
        let Some((name, value)) = attrs.next().unwrap_or("").split_once('=') else {
            return false;
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || name.bytes().any(|b| b <= 0x20 || b == b'=') {
            return false;
        }
        // Per-cookie size cap (the spec's 4096-byte suggestion): without it a
        // hostile server could park ~12 MB in a full jar and the disk file.
        if name.len() + value.len() > 4096 {
            return false;
        }
        let mut domain_attr: Option<String> = None;
        let mut path_attr: Option<String> = None;
        let mut secure = false;
        let mut http_only = false;
        let mut expires: Option<i64> = None;
        let mut max_age: Option<i64> = None;
        for attr in attrs {
            let (k, v) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (attr.trim(), ""),
            };
            match k.to_ascii_lowercase().as_str() {
                "domain" => {
                    let d = v.trim_start_matches('.').to_ascii_lowercase();
                    if !d.is_empty() {
                        domain_attr = Some(d);
                    }
                }
                "path" => {
                    if v.starts_with('/') {
                        path_attr = Some(v.to_string());
                    }
                }
                "expires" => expires = parse_cookie_date(v),
                "max-age" => {
                    if let Ok(n) = v.parse::<i64>() {
                        max_age = Some(n);
                    }
                }
                "secure" => secure = true,
                "httponly" => http_only = true,
                // SameSite / Priority / extensions: documented no-ops
                _ => {}
            }
        }
        let (domain, host_only) = match domain_attr {
            Some(d) => {
                if !domain_match(&u.host, &d) {
                    return false; // a page cannot set cookies for a foreign host
                }
                (d, false)
            }
            None => (u.host.clone(), true),
        };
        let path = path_attr.unwrap_or_else(|| default_path(&u.path));
        // `Max-Age` wins over `Expires` (RFC 6265 §5.3); <= 0 deletes.
        let expires = match max_age {
            Some(n) if n <= 0 => return self.remove(&name, &domain, &path),
            Some(n) => Some(now.saturating_add(n)),
            None => expires,
        };
        if expires.map(|e| e <= now).unwrap_or(false) {
            return self.remove(&name, &domain, &path);
        }
        // Replacing keeps the original creation stamp (spec), which is what
        // the send-order tiebreak below relies on.
        let created = self
            .cookies
            .iter()
            .find(|c| c.same_key(&name, &domain, &path))
            .map(|c| c.created)
            .unwrap_or(now);
        self.cookies
            .retain(|c| !c.same_key(&name, &domain, &path));
        self.cookies.push(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain,
            host_only,
            path,
            secure,
            http_only,
            expires,
            created,
        });
        self.evict(now);
        true
    }

    /// `Cookie` request header value for `url`, or None when nothing matches.
    pub fn header(&self, url: &str, now: i64) -> Option<String> {
        self.join_matching(url, now, false)
    }

    /// `document.cookie` read: HttpOnly cookies are hidden from script.
    pub fn js_get(&self, url: &str, now: i64) -> String {
        self.join_matching(url, now, true).unwrap_or_default()
    }

    /// `document.cookie` write: parsed like a `Set-Cookie` value whose
    /// default path is the document's directory. Script cannot create an
    /// HttpOnly cookie (RFC 6265 §5.2), so that attribute is dropped first.
    pub fn js_set(&mut self, url: &str, line: &str, now: i64) -> bool {
        let cleaned: Vec<&str> = line
            .split(';')
            .filter(|p| !p.trim().eq_ignore_ascii_case("httponly"))
            .collect();
        self.set_cookie(url, &cleaned.join(";"), now)
    }

    fn join_matching(&self, url: &str, now: i64, for_script: bool) -> Option<String> {
        let u = UrlParts::parse(url)?;
        let mut hits: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.matches(&u, now, for_script))
            .collect();
        // longer paths first, then oldest first (RFC 6265 §5.4)
        hits.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created.cmp(&b.created))
        });
        if hits.is_empty() {
            return None;
        }
        let mut out = String::new();
        for c in hits {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&c.name);
            out.push('=');
            out.push_str(&c.value);
        }
        Some(out)
    }

    fn remove(&mut self, name: &str, domain: &str, path: &str) -> bool {
        let before = self.cookies.len();
        self.cookies
            .retain(|c| !c.same_key(name, domain, path));
        self.cookies.len() != before
    }

    /// Drop expired cookies, then the oldest-created ones over the cap.
    fn evict(&mut self, now: i64) {
        if self.cookies.len() <= self.cap {
            return;
        }
        self.cookies
            .retain(|c| c.expires.map(|e| e > now).unwrap_or(true));
        if self.cookies.len() > self.cap {
            self.cookies.sort_by_key(|c| c.created);
            let excess = self.cookies.len() - self.cap;
            self.cookies.drain(..excess);
        }
    }

    /// Persisted form: a magic line then one tab-separated cookie per line.
    pub fn to_text(&self) -> String {
        let mut s = String::from(FILE_HEADER);
        s.push('\n');
        for c in &self.cookies {
            let fields = [
                esc(&c.name),
                esc(&c.value),
                esc(&c.domain),
                if c.host_only { "1" } else { "0" }.to_string(),
                esc(&c.path),
                if c.secure { "1" } else { "0" }.to_string(),
                if c.http_only { "1" } else { "0" }.to_string(),
                c.expires.map(|e| e.to_string()).unwrap_or_default(),
                c.created.to_string(),
            ];
            s.push_str(&fields.join("\t"));
            s.push('\n');
        }
        s
    }

    /// Load a [`to_text`] payload; malformed lines are skipped, never fatal.
    pub fn from_text(&mut self, text: &str) {
        let mut lines = text.lines();
        if lines.next() != Some(FILE_HEADER) {
            return;
        }
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 9 {
                continue;
            }
            let (Ok(created), Ok(expires)) = (
                f[8].parse::<i64>(),
                if f[7].is_empty() {
                    Ok(None)
                } else {
                    f[7].parse::<i64>().map(Some)
                },
            ) else {
                continue;
            };
            let domain = unesc(f[2]);
            let path = unesc(f[4]);
            let name = unesc(f[0]);
            if name.is_empty() || domain.is_empty() || !path.starts_with('/') {
                continue;
            }
            self.cookies.push(Cookie {
                name,
                value: unesc(f[1]),
                domain,
                host_only: f[3] == "1",
                path,
                secure: f[5] == "1",
                http_only: f[6] == "1",
                expires,
                created,
            });
        }
        self.evict(now_secs());
    }
}

impl Cookie {
    fn matches(&self, u: &UrlParts, now: i64, for_script: bool) -> bool {
        if self.expires.map(|e| e <= now).unwrap_or(false) {
            return false;
        }
        if for_script && self.http_only {
            return false;
        }
        if self.secure && !u.secure {
            return false;
        }
        let domain_ok = if self.host_only {
            u.host == self.domain
        } else {
            domain_match(&u.host, &self.domain)
        };
        domain_ok && path_match(&u.path, &self.path)
    }
}

// ---------------------------------------------------------------------------
// URL + matching helpers
// ---------------------------------------------------------------------------

struct UrlParts {
    secure: bool,
    host: String,
    path: String,
}

impl UrlParts {
    fn parse(url: &str) -> Option<UrlParts> {
        let (scheme, rest) = url.split_once("://")?;
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let host = if let Some(v6) = authority.strip_prefix('[') {
            v6.split(']').next().unwrap_or("").to_string()
        } else {
            authority.split(':').next().unwrap_or("").to_string()
        };
        if host.is_empty() {
            return None;
        }
        let path = tail.split(['?', '#']).next().unwrap_or("/");
        Some(UrlParts {
            secure: scheme.eq_ignore_ascii_case("https"),
            host: host.to_ascii_lowercase(),
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
        })
    }
}

/// Host matches a cookie domain: equal, or a dot-separated suffix of it.
fn domain_match(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// Request path falls inside the cookie path (RFC 6265 §5.1.4).
fn path_match(req: &str, cookie: &str) -> bool {
    req == cookie
        || (req.starts_with(cookie)
            && (cookie.ends_with('/')
                || req.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// Default cookie path: the request path minus its last segment.
fn default_path(req_path: &str) -> String {
    match req_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => req_path[..i].to_string(),
    }
}

// ---------------------------------------------------------------------------
// Date parsing (RFC 1123 only - see module shortcuts)
// ---------------------------------------------------------------------------

fn month_from(tok: &str) -> Option<i64> {
    let m = tok.get(..3)?.to_ascii_lowercase();
    Some(match m.as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` -> unix seconds.
fn parse_cookie_date(s: &str) -> Option<i64> {
    let (mut day, mut month, mut year, mut time) = (None, None, None, None);
    for tok in s.split(|c: char| c.is_whitespace() || c == ',' || c == '-') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains(':') {
            let mut it = t.split(':');
            let h: i64 = it.next()?.parse().ok()?;
            let mi: i64 = it.next()?.parse().ok()?;
            let sec: i64 = it.next()?.parse().ok()?;
            if h > 23 || mi > 59 || sec > 60 {
                return None;
            }
            time = Some((h, mi, sec));
        } else if month.is_none() && month_from(t).is_some() {
            month = month_from(t);
        } else if let Ok(n) = t.parse::<i64>() {
            if day.is_none() && (1..=31).contains(&n) {
                day = Some(n);
            } else if year.is_none() {
                year = Some(n);
            }
        }
    }
    let (d, m, mut y) = (day?, month?, year?);
    // two-digit years: 70-99 -> 1900s, 0-69 -> 2000s (RFC 6265 §5.1.1)
    if (0..100).contains(&y) {
        y += if y >= 70 { 1900 } else { 2000 };
    }
    if !(1601..=9999).contains(&y) || !(1..=12).contains(&m) {
        return None;
    }
    let (h, mi, sec) = time?;
    Some(days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// civil-from-days inverse; no chrono dependency for one formula).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Persistence + the process-wide jar
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cookie_path() -> PathBuf {
    crate::cache_dir().join("cookies")
}

fn global() -> &'static Mutex<Jar> {
    static JAR: OnceLock<Mutex<Jar>> = OnceLock::new();
    JAR.get_or_init(|| {
        let mut jar = Jar::new();
        if let Ok(text) = std::fs::read_to_string(cookie_path()) {
            jar.from_text(&text);
        }
        Mutex::new(jar)
    })
}

/// A mutation is pending a disk write. Kept separate from the time gate so
/// a change made inside the same second as the last write is DEFERRED (the
/// UI loop calls [`flush`]) rather than dropped — dropping it lost a
/// script's `document.setCookie` on every page that also received a
/// `Set-Cookie`, which is exactly the login flow.
static DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_SAVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Mark dirty and write if the one-second gate allows it.
fn save(jar: &Jar) {
    DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
    flush_locked(jar);
}

/// Write the jar back at most once per second. A page that sets cookies in
/// a loop cannot turn into a disk-write loop; changes made in the meantime
/// stay marked and go out on the next flush (the UI loop calls this every
/// frame, so a live process loses nothing).
fn flush_locked(jar: &Jar) {
    use std::sync::atomic::Ordering;
    if !DIRTY.load(Ordering::Relaxed) {
        return;
    }
    let now = now_secs();
    let last = LAST_SAVE.load(Ordering::Relaxed);
    if now <= last {
        return;
    }
    if LAST_SAVE
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    DIRTY.store(false, Ordering::Relaxed);
    let dir = crate::cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(cookie_path(), jar.to_text());
}

/// Periodic flush hook for the shell's frame loop: writes pending changes
/// once the one-second gate opens. Cheap when nothing is pending.
pub fn flush() {
    if DIRTY.load(std::sync::atomic::Ordering::Relaxed) {
        flush_locked(&lock());
    }
}

fn lock() -> std::sync::MutexGuard<'static, Jar> {
    global().lock().unwrap_or_else(|e| e.into_inner())
}

/// `Cookie` header for an outgoing request, if the jar has matches.
/// Logging is opt-in (`MAZDA_COOKIE_LOG=1`) because subresources would
/// otherwise print a line each.
pub fn request_header(url: &str) -> Option<String> {
    let h = lock().header(url, now_secs());
    if let Some(h) = &h {
        if std::env::var_os("MAZDA_COOKIE_LOG").is_some() {
            let n = h.split(';').count();
            eprintln!("[cookie] send {n} to {url}");
        }
    }
    h
}

/// Store every `Set-Cookie` value from a response to `url`.
pub fn store_response(url: &str, headers: &[String]) {
    let mut jar = lock();
    let now = now_secs();
    let mut changed = 0usize;
    for h in headers {
        if jar.set_cookie(url, h, now) {
            changed += 1;
        }
    }
    if changed > 0 {
        save(&jar);
        if let Some(u) = UrlParts::parse(url) {
            eprintln!("[cookie] set {changed} for {}", u.host);
        }
    }
}

/// `document.getCookie()` - the script-visible subset (no HttpOnly).
pub fn document_get(url: &str) -> String {
    lock().js_get(url, now_secs())
}

/// `document.setCookie(str)` - same parser as a `Set-Cookie` value.
pub fn document_set(url: &str, line: &str) {
    let mut jar = lock();
    if jar.js_set(url, line, now_secs()) {
        save(&jar);
    }
}

// ---------------------------------------------------------------------------
// File escaping
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let a = it.next();
        let b = it.next();
        match (a, b) {
            (Some('2'), Some('5')) => out.push('%'),
            (Some('0'), Some('9')) => out.push('\t'),
            (Some('0'), Some('A')) => out.push('\n'),
            (Some('0'), Some('D')) => out.push('\r'),
            (Some(x), Some(y)) => {
                out.push('%');
                out.push(x);
                out.push(y);
            }
            (Some(x), None) => {
                out.push('%');
                out.push(x);
            }
            (None, _) => out.push('%'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar() -> Jar {
        Jar::new()
    }

    #[test]
    fn set_and_send_respects_path() {
        let mut j = jar();
        assert!(j.set_cookie("https://example.com/a/b", "sid=abc; Path=/a", 100));
        assert_eq!(
            j.header("https://example.com/a/c", 101).as_deref(),
            Some("sid=abc")
        );
        assert_eq!(j.header("https://example.com/x", 101), None);
        // default path is the request directory, not "/"
        assert!(j.set_cookie("https://example.com/dir/page", "k=v", 102));
        assert_eq!(j.header("https://example.com/dir/other", 103).as_deref(), Some("k=v"));
        assert_eq!(j.header("https://example.com/a/other", 103).as_deref(), Some("sid=abc"));
        assert_eq!(j.header("https://example.com/", 103), None);
    }

    #[test]
    fn domain_attr_covers_subdomains_but_not_siblings() {
        let mut j = jar();
        assert!(j.set_cookie("https://www.example.com/", "a=1; Domain=example.com", 1));
        assert_eq!(
            j.header("https://shop.example.com/", 2).as_deref(),
            Some("a=1")
        );
        assert_eq!(j.header("https://example.com/", 2).as_deref(), Some("a=1"));
        assert_eq!(j.header("https://notexample.com/", 2), None);
        // a foreign Domain attribute is refused outright
        assert!(!j.set_cookie("https://example.com/", "b=2; Domain=evil.com", 3));
        // host-only cookies never leak to a subdomain
        assert!(j.set_cookie("https://example.com/", "c=3", 4));
        assert_eq!(j.header("https://sub.example.com/", 5).as_deref(), Some("a=1"));
    }

    #[test]
    fn secure_cookies_only_go_over_https() {
        let mut j = jar();
        assert!(j.set_cookie("https://example.com/", "t=1; Secure", 1));
        assert_eq!(j.header("https://example.com/", 2).as_deref(), Some("t=1"));
        assert_eq!(j.header("http://example.com/", 2), None);
    }

    #[test]
    fn http_only_is_hidden_from_script() {
        let mut j = jar();
        j.set_cookie("https://example.com/", "sid=1; HttpOnly", 1);
        j.set_cookie("https://example.com/", "pref=dark", 2);
        assert_eq!(j.header("https://example.com/", 3).as_deref(), Some("sid=1; pref=dark"));
        assert_eq!(j.js_get("https://example.com/", 3), "pref=dark");
    }

    #[test]
    fn max_age_zero_and_past_expiry_delete() {
        let mut j = jar();
        j.set_cookie("https://example.com/", "a=1", 1);
        j.set_cookie("https://example.com/", "b=2", 1);
        assert!(j.set_cookie("https://example.com/", "a=; Max-Age=0", 2));
        assert_eq!(j.header("https://example.com/", 3).as_deref(), Some("b=2"));
        // already in the past relative to `now` -> deletes, like Max-Age <= 0
        assert!(j.set_cookie(
            "https://example.com/",
            "b=2; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            3
        ));
        assert_eq!(j.header("https://example.com/", 4), None);
    }

    #[test]
    fn expiry_in_the_future_keeps_the_cookie() {
        let mut j = jar();
        // 2015-10-21 07:28:00 UTC = 1445412480
        j.set_cookie(
            "https://example.com/",
            "keep=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT",
            1,
        );
        assert_eq!(j.header("https://example.com/", 1_445_412_479).as_deref(), Some("keep=1"));
        assert_eq!(j.header("https://example.com/", 1_445_412_481), None);
    }

    #[test]
    fn longer_paths_come_first_then_creation_order() {
        let mut j = jar();
        j.set_cookie("https://example.com/", "root=1", 1);
        j.set_cookie("https://example.com/a/b", "deep=1; Path=/a/b", 2);
        j.set_cookie("https://example.com/a", "mid=1; Path=/a", 3);
        assert_eq!(
            j.header("https://example.com/a/b/c", 4).as_deref(),
            Some("deep=1; mid=1; root=1")
        );
    }

    #[test]
    fn cap_evicts_oldest_and_expired_first() {
        let mut j = jar();
        j.cap = 3;
        for i in 0..5 {
            j.set_cookie("https://example.com/", &format!("k{i}=v"), i as i64);
        }
        assert_eq!(j.cookies.len(), 3);
        let names: Vec<&str> = j.cookies.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"k2") && names.contains(&"k3") && names.contains(&"k4"));
        // expired ones are dropped before live ones regardless of age
        let mut j = jar();
        j.cap = 2;
        j.set_cookie("https://example.com/", "old=1; Max-Age=1", 100);
        j.set_cookie("https://example.com/", "live=1", 100);
        j.set_cookie("https://example.com/", "new=1", 101);
        j.set_cookie("https://example.com/", "newer=1", 102);
        let names: Vec<&str> = j.cookies.iter().map(|c| c.name.as_str()).collect();
        assert!(!names.contains(&"old"), "expired evicted first: {names:?}");
    }

    #[test]
    fn text_round_trip_preserves_cookies() {
        let mut j = jar();
        j.set_cookie("https://example.com/a", "sid=abc; Path=/a; Secure; HttpOnly; Max-Age=60", 10);
        j.set_cookie("https://example.com/", "tab=v\t1", 11);
        let text = j.to_text();
        let mut back = jar();
        back.from_text(&text);
        assert_eq!(back.cookies, j.cookies);
        // and the restored jar still sends them
        assert_eq!(
            back.header("https://example.com/a/x", 12).as_deref(),
            Some("sid=abc; tab=v\t1")
        );
    }

    #[test]
    fn corrupt_persisted_lines_are_skipped() {
        let mut j = jar();
        j.from_text("MCOOKIE1\nnot-a-cookie\n\t\t\t\t\t\t\t\t\nx\ty\texample.com\t1\t/\t0\t0\t\t5\n");
        assert_eq!(j.cookies.len(), 1);
        assert_eq!(j.cookies[0].name, "x");
        // a wrong magic line means "not ours": load nothing
        let mut j = jar();
        j.from_text("OTHER1\nx\ty\texample.com\t1\t/\t0\t0\t\t5\n");
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn js_set_uses_the_document_directory_and_js_get_hides_http_only() {
        let mut j = jar();
        j.js_set("https://example.com/docs/page", "a=1", 1);
        assert_eq!(j.js_get("https://example.com/docs/other", 2), "a=1");
        assert_eq!(j.js_get("https://example.com/", 2), "");
        // a JS write cannot mark a cookie HttpOnly
        j.js_set("https://example.com/docs/page", "b=2; HttpOnly", 3);
        assert_eq!(j.js_get("https://example.com/docs/other", 4), "a=1; b=2");
    }

    #[test]
    fn oversized_cookies_are_rejected() {
        let mut j = jar();
        let big = format!("k={}", "v".repeat(5000));
        assert!(!j.set_cookie("https://example.com/", &big, 1));
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn malformed_set_cookie_lines_are_ignored() {
        let mut j = jar();
        assert!(!j.set_cookie("https://example.com/", "novalue", 1));
        assert!(!j.set_cookie("https://example.com/", "=empty", 1));
        assert!(!j.set_cookie("https://example.com/", "bad name=v", 1));
        assert!(j.cookies.is_empty());
    }

    #[test]
    fn url_parts_handle_ports_userinfo_and_ipv6() {
        let u = UrlParts::parse("https://user:pw@example.com:8443/a/b?q=1#f").unwrap();
        assert_eq!((u.secure, u.host.as_str(), u.path.as_str()), (true, "example.com", "/a/b"));
        let u = UrlParts::parse("http://[::1]:80/x").unwrap();
        assert_eq!((u.secure, u.host.as_str(), u.path.as_str()), (false, "::1", "/x"));
        assert!(UrlParts::parse("data:text/html,x").is_none());
    }
}
