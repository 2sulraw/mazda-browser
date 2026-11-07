//! `URL` / `URLSearchParams` support.
//!
//! Parsing and serialization go through the general-purpose `url` crate (the
//! same one navigation uses) rather than a hand-written parser: URL syntax is
//! a large, security-sensitive grammar, and the crate is already a dependency.
//! The DOM-shaped accessors built on top are this module's own.
//!
//! Honest limits (see the wave notes in PLAN.md):
//! - `URL` instances snapshot their components at construction: assigning
//!   `u.pathname = …` or `u.href = …` does not re-parse (no accessor hooks in
//!   the interpreter).
//! - `u.searchParams` is a snapshot too; mutating it does not rewrite
//!   `u.href` (the real API keeps them live-linked).
//! - `URL.createObjectURL` / `revokeObjectURL` are absent: there are no blobs.

/// The DOM-visible parts of a parsed absolute URL.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Parts {
    pub href: String,
    /// `https:` — the scheme with its trailing colon
    pub protocol: String,
    /// `example.com:8080` (port omitted when it is the scheme default)
    pub host: String,
    pub hostname: String,
    /// empty when the URL has no explicit non-default port
    pub port: String,
    /// always starts with `/` for special schemes
    pub pathname: String,
    /// `?a=1`, empty when there is no query
    pub search: String,
    /// `#frag`, empty when there is no fragment
    pub hash: String,
    /// `https://example.com:8080`, or `null` for an opaque origin
    pub origin: String,
    pub username: String,
    pub password: String,
}

/// Parse an absolute URL into its DOM-facing components.
pub(crate) fn parts(abs: &str) -> Option<Parts> {
    let u = url::Url::parse(abs).ok()?;
    let hostname = u.host_str().unwrap_or("").to_string();
    let port = u.port().map(|p| p.to_string()).unwrap_or_default();
    let host = if port.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port}")
    };
    Some(Parts {
        href: u.as_str().to_string(),
        protocol: format!("{}:", u.scheme()),
        host,
        hostname,
        port,
        pathname: u.path().to_string(),
        search: match u.query() {
            Some(q) => format!("?{q}"),
            None => String::new(),
        },
        hash: match u.fragment() {
            Some(f) => format!("#{f}"),
            None => String::new(),
        },
        origin: u.origin().ascii_serialization(),
        username: u.username().to_string(),
        password: u.password().unwrap_or("").to_string(),
    })
}

/// Resolve `href` against `base` without the navigation layer's restrictions
/// (any scheme is allowed here; the URL API is not a navigation request).
/// Returns the serialized absolute URL.
pub(crate) fn join(base: &str, href: &str) -> Option<String> {
    let b = url::Url::parse(base).ok()?;
    b.join(href).ok().map(|u| u.to_string())
}

/// Decode a query string (a leading `?` is tolerated) into decoded pairs.
pub(crate) fn parse_query(q: &str) -> Vec<(String, String)> {
    let q = q.strip_prefix('?').unwrap_or(q);
    url::form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Encode pairs as `application/x-www-form-urlencoded` (space → `+`).
pub(crate) fn encode_pairs(pairs: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_absolute() {
        let p = parts("https://user:secret@example.com:8443/a/b?x=1&y=2#frag").unwrap();
        assert_eq!(p.protocol, "https:");
        assert_eq!(p.hostname, "example.com");
        assert_eq!(p.port, "8443");
        assert_eq!(p.host, "example.com:8443");
        assert_eq!(p.pathname, "/a/b");
        assert_eq!(p.search, "?x=1&y=2");
        assert_eq!(p.hash, "#frag");
        assert_eq!(p.origin, "https://example.com:8443");
        assert_eq!(p.username, "user");
        assert_eq!(p.password, "secret");
    }

    #[test]
    fn default_port_is_hidden() {
        let p = parts("http://example.com:80/").unwrap();
        assert_eq!(p.port, "");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.origin, "http://example.com");
    }

    #[test]
    fn join_relative() {
        assert_eq!(
            join("https://example.com/a/b", "../c?q=1").as_deref(),
            Some("https://example.com/c?q=1")
        );
        assert_eq!(
            join("https://example.com/", "//other.test/x").as_deref(),
            Some("https://other.test/x")
        );
    }

    #[test]
    fn query_round_trip() {
        let pairs = parse_query("?a=1&b=hello+world&b=%C3%A9");
        assert_eq!(pairs[0], ("a".into(), "1".into()));
        assert_eq!(pairs[1], ("b".into(), "hello world".into()));
        assert_eq!(pairs[2], ("b".into(), "é".into()));
        assert_eq!(encode_pairs(&pairs), "a=1&b=hello+world&b=%C3%A9");
    }
}
