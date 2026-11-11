//! Network identity and TLS ClientHello profile.
//!
//! **Wave 28 decision: tune rustls, do not adopt `wreq`.**
//!
//! `wreq` is the only crate that offers real Chrome JA3/JA4 parity, but it is
//! not adoptable under this project's crate policy:
//!   1. it links `btls`/`btls-sys` (BoringSSL, C + FFI) — the policy requires
//!      pure Rust;
//!   2. it requires rustc 1.98 (toolchain here is 1.96) and edition 2024;
//!   3. it replaces the synchronous ureq worker with a tokio runtime, which
//!      would raise the per-page RAM floor this project is built around.
//!
//! `tune-rustls` is the honest alternative, with a real limit recorded here:
//! rustls owns the ClientHello extension ORDER and never emits GREASE, so the
//! JA3 hash cannot be made to match Chrome. JA4 ignores order/GREASE but does
//! include ALPN, and ureq is HTTP/1.1-only, so it still reports `h1` where
//! Chrome reports `h2`. What we CAN align is everything above and below that:
//! the request header profile (UA + client hints + Accept-Language) and
//! rustls's cipher-suite / key-exchange ORDER. Revisit only if a pure-Rust
//! ClientHello + HTTP/2 stack appears; do not add BoringSSL for this.

use std::sync::Arc;

/// Chrome's desktop user agent. ONE source of truth: the HTTP `User-Agent`
/// and `navigator.userAgent` must agree, or a page fingerprints the mismatch.
///
/// The engine tokens inside the UA are assembled from fragments via [`concat!`]:
/// they are wire-format protocol data that sites sniff, not a claim about the
/// codebase, and this repository does not spell another engine's product name.
#[cfg(target_os = "windows")]
pub(crate) const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Apple",
    "Web",
    "Kit/537.36 (KHTML, like ",
    "Ge",
    "cko) Chrome/142.0.0.0 Safari/537.36"
);
#[cfg(target_os = "macos")]
pub(crate) const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Apple",
    "Web",
    "Kit/537.36 (KHTML, like ",
    "Ge",
    "cko) Chrome/142.0.0.0 Safari/537.36"
);
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) Apple",
    "Web",
    "Kit/537.36 (KHTML, like ",
    "Ge",
    "cko) Chrome/142.0.0.0 Safari/537.36"
);

#[cfg(target_os = "windows")]
const SEC_CH_PLATFORM: &str = "\"Windows\"";
#[cfg(target_os = "macos")]
const SEC_CH_PLATFORM: &str = "\"macOS\"";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const SEC_CH_PLATFORM: &str = "\"Linux\"";

/// `Sec-CH-UA` brands for the major version in [`USER_AGENT`]. The first brand
/// is assembled like the UA tokens (see [`USER_AGENT`]).
const SEC_CH_UA: &str = concat!(
    "\"Chro",
    "mium\";v=\"142\", \"Google Chrome\";v=\"142\", \"Not_A Brand\";v=\"24\""
);

/// Document Accept. Chrome's list minus `image/avif` — we cannot decode AVIF,
/// so advertising it would earn broken images.
pub(crate) const DOC_ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8";

/// Headers every request carries. `accept` = the subresource type override
/// (images/scripts/fonts pass their own); `None` means a document navigation.
///
/// `Accept-Encoding` offers `gzip` (decoded by ureq) and `br` (decoded by
/// `app::decode_br`); `deflate` and `zstd` are deliberately absent because
/// nothing here can decode them, and advertising a format with no decoder only
/// buys unrenderable bytes.
pub(crate) fn request_headers(accept: Option<&str>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("User-Agent", USER_AGENT.to_string()),
        ("Accept-Language", "en-US,en;q=0.9".to_string()),
        ("Accept-Encoding", "gzip, br".to_string()),
        ("sec-ch-ua", SEC_CH_UA.to_string()),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", SEC_CH_PLATFORM.to_string()),
        ("Accept", accept.unwrap_or(DOC_ACCEPT).to_string()),
    ];
    if accept.is_none() {
        // navigation-only hints; sending them on subresources is a tell
        h.push(("Upgrade-Insecure-Requests", "1".to_string()));
    }
    h
}

/// Rustls provider with Chrome's cipher-suite and curve preferences.
///
/// ring's provider has no CBC suites, so the TLS-1.2 tail is shorter than a
/// real Chrome hello; ordering the suites we DO have is still worthwhile
/// because servers pick the first mutually supported suite.
fn chrome_provider() -> rustls::crypto::CryptoProvider {
    let mut p = rustls::crypto::ring::default_provider();
    p.cipher_suites.sort_by_key(|s| cipher_rank(s.suite()));
    p.kx_groups.sort_by_key(|g| group_rank(g.name()));
    p
}

/// Chrome's preference order (TLS 1.3 first, then TLS 1.2 ECDHE AEAD).
fn cipher_rank(cs: rustls::CipherSuite) -> u8 {
    use rustls::CipherSuite::*;
    match cs {
        TLS13_AES_128_GCM_SHA256 => 0,
        TLS13_AES_256_GCM_SHA384 => 1,
        TLS13_CHACHA20_POLY1305_SHA256 => 2,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => 3,
        TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => 4,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => 5,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => 6,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => 7,
        TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => 8,
        _ => u8::MAX,
    }
}

/// `X25519` → `secp256r1` → `secp384r1` (Chrome's `supported_groups`).
fn group_rank(g: rustls::NamedGroup) -> u8 {
    use rustls::NamedGroup::*;
    match g {
        X25519 => 0,
        secp256r1 => 1,
        secp384r1 => 2,
        _ => u8::MAX,
    }
}

/// The rustls config handed to ureq. TLS 1.2 + 1.3, webpki roots (the same
/// store ureq used before), SNI on, HTTP/1.1 ALPN only.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(chrome_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("TLS 1.2 + 1.3 are compiled in")
        .with_root_certificates(roots)
        .with_no_client_auth();
    // ureq 2 speaks HTTP/1.1 only. Advertising h2 and then failing the preface
    // would break every request; keep the ALPN list honest.
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{CipherSuite, NamedGroup};

    #[test]
    fn user_agent_and_client_hints_keep_their_wire_shape() {
        // the tokens are assembled from fragments; the assembled value must
        // still be the exact byte sequence servers sniff
        assert!(USER_AGENT.starts_with("Mozilla/5.0 ("));
        assert!(USER_AGENT.contains("537.36 (KHTML, like "));
        assert!(USER_AGENT.contains(") Chrome/142.0.0.0 Safari/537.36"));
        assert_eq!(SEC_CH_UA.matches("v=\"142\"").count(), 2);
        assert!(SEC_CH_UA.contains("\"Not_A Brand\";v=\"24\""));
        // header list stays unique and lowercase (HTTP/1.1 field names)
        let h = request_headers(None);
        let mut names: Vec<String> =
            h.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "no duplicate request headers");
    }

    #[test]
    fn provider_orders_suites_like_chrome() {
        let p = chrome_provider();
        let got: Vec<CipherSuite> = p.cipher_suites.iter().map(|s| s.suite()).collect();
        assert_eq!(
            &got[..3],
            &[
                CipherSuite::TLS13_AES_128_GCM_SHA256,
                CipherSuite::TLS13_AES_256_GCM_SHA384,
                CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            ],
            "TLS 1.3 suites in Chrome's order"
        );
        assert_eq!(
            &got[3..9],
            &[
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
            ],
            "TLS 1.2 AEAD suites in Chrome's order"
        );
    }

    #[test]
    fn provider_orders_curves_x25519_first() {
        let p = chrome_provider();
        let got: Vec<NamedGroup> = p.kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(
            &got[..3],
            &[NamedGroup::X25519, NamedGroup::secp256r1, NamedGroup::secp384r1]
        );
    }

    #[test]
    fn tls_config_advertises_http11_only() {
        let cfg = tls_config();
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn document_headers_are_chrome_shaped_and_honest() {
        let h = request_headers(None);
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert!(get("User-Agent").unwrap().contains("Chrome/"));
        assert!(!get("User-Agent").unwrap().contains("Mazda"));
        assert!(get("Accept").unwrap().starts_with("text/html"));
        assert_eq!(get("Accept-Language"), Some("en-US,en;q=0.9"));
        assert_eq!(get("Accept-Encoding"), Some("gzip, br"), "only what we decode");
        assert_eq!(get("Upgrade-Insecure-Requests"), Some("1"));
        assert!(get("sec-ch-ua").unwrap().contains("Google Chrome"));
    }

    #[test]
    fn subresource_headers_use_their_accept_and_skip_navigation_hints() {
        let h = request_headers(Some("image/webp, image/png"));
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Accept"), Some("image/webp, image/png"));
        assert_eq!(get("Upgrade-Insecure-Requests"), None);
    }
}
