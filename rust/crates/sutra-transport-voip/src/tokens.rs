//! Per-call media tokens for the PCM bridge.
//!
//! A PBX's bridge client sends no custom headers, so the bridge URL is the only place its
//! credential can ride. Rather than the deployment's long-lived media token, each bridge URL
//! carries a token minted for that one call when the call is handed to the bridge:
//!
//! ```text
//! v1.<expiry, unix seconds>.<nonce, 16 hex>.<HMAC-SHA256, base64url>
//! ```
//!
//! The HMAC covers the call id, the expiry and the nonce, under a key derived from the media
//! token (so every replica sharing the media token verifies it). A token is refused when it is
//! malformed, its MAC does not match (forged, or minted for another call), it has expired, or
//! it was used before — each token opens one bridge socket.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The token format's version tag.
pub const TOKEN_VERSION: &str = "v1";
/// How long a minted bridge token is valid, by default.
pub const DEFAULT_TOKEN_TTL_SECONDS: u64 = 60;
/// The most used tokens remembered (expired ones are dropped first).
const USED_MEMORY: usize = 65_536;

/// Why a bridge token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRefusal {
    Missing,
    Malformed,
    /// The MAC does not match: forged, tampered, or minted for another call.
    Forged,
    Expired,
    /// Already used once.
    Reused,
}

impl TokenRefusal {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenRefusal::Missing => "missing",
            TokenRefusal::Malformed => "malformed",
            TokenRefusal::Forged => "forged",
            TokenRefusal::Expired => "expired",
            TokenRefusal::Reused => "reused",
        }
    }
}

/// Mints and verifies bridge tokens.
pub struct BridgeTokens {
    key: Vec<u8>,
    ttl_seconds: u64,
    /// nonce → expiry, for the single-use check.
    used: Mutex<HashMap<String, u64>>,
}

fn now_unix() -> u64 {
    time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}

impl BridgeTokens {
    /// Tokens keyed from the deployment's media token.
    pub fn new(media_token: &str, ttl_seconds: u64) -> BridgeTokens {
        let mut mac =
            HmacSha256::new_from_slice(media_token.as_bytes()).expect("HMAC takes any key length");
        mac.update(b"sutra-voip-bridge-token-v1");
        BridgeTokens {
            key: mac.finalize().into_bytes().to_vec(),
            ttl_seconds: ttl_seconds.max(1),
            used: Mutex::new(HashMap::new()),
        }
    }

    fn mac(&self, call_id: &str, expiry: u64, nonce: &str) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(call_id.as_bytes());
        mac.update(b"\x1f");
        mac.update(expiry.to_string().as_bytes());
        mac.update(b"\x1f");
        mac.update(nonce.as_bytes());
        mac
    }

    /// A token for `call_id` (empty for a bridge socket that will name no call).
    pub fn mint(&self, call_id: &str) -> String {
        self.mint_at(call_id, now_unix())
    }

    pub fn mint_at(&self, call_id: &str, now: u64) -> String {
        let expiry = now + self.ttl_seconds;
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
        let tag = self.mac(call_id, expiry, &nonce).finalize().into_bytes();
        format!(
            "{TOKEN_VERSION}.{expiry}.{nonce}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag)
        )
    }

    /// Verify (and spend) `token` for `call_id`.
    pub fn verify(&self, token: Option<&str>, call_id: &str) -> Result<(), TokenRefusal> {
        self.verify_at(token, call_id, now_unix())
    }

    pub fn verify_at(
        &self,
        token: Option<&str>,
        call_id: &str,
        now: u64,
    ) -> Result<(), TokenRefusal> {
        let token = token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(TokenRefusal::Missing)?;
        let mut parts = token.split('.');
        let (Some(version), Some(expiry), Some(nonce), Some(tag), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(TokenRefusal::Malformed);
        };
        if version != TOKEN_VERSION
            || nonce.len() != 16
            || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(TokenRefusal::Malformed);
        }
        let expiry: u64 = expiry.parse().map_err(|_| TokenRefusal::Malformed)?;
        let tag = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(tag)
            .map_err(|_| TokenRefusal::Malformed)?;
        // Constant-time comparison.
        self.mac(call_id, expiry, nonce)
            .verify_slice(&tag)
            .map_err(|_| TokenRefusal::Forged)?;
        if now > expiry {
            return Err(TokenRefusal::Expired);
        }
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        if used.len() >= USED_MEMORY {
            used.retain(|_, until| *until >= now);
        }
        if used.contains_key(nonce) {
            return Err(TokenRefusal::Reused);
        }
        used.insert(nonce.to_string(), expiry);
        Ok(())
    }
}

/// `url` with its query string replaced by `?…` — for log lines, which never carry a token.
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?…"),
        None => url.to_string(),
    }
}

/// `text` with the value of every `token=` parameter replaced by `…` (an error message that
/// echoes a URL).
pub fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("token=") {
        out.push_str(&rest[..at + "token=".len()]);
        out.push('…');
        let after = &rest[at + "token=".len()..];
        let end = after
            .find(|c: char| c == '&' || c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// The bearer credential of an `Authorization: Bearer <token>` header value.
pub fn bearer(value: Option<&str>) -> Option<&str> {
    let value = value?.trim();
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_opens_its_call_once_until_it_expires() {
        let tokens = BridgeTokens::new("media-secret", 60);
        let now = 1_000_000;
        let token = tokens.mint_at("c1", now);
        assert!(!token.contains("media-secret"));
        assert_eq!(
            tokens.verify_at(Some(&token), "c2", now),
            Err(TokenRefusal::Forged),
            "another call"
        );
        assert_eq!(tokens.verify_at(Some(&token), "c1", now + 10), Ok(()));
        assert_eq!(
            tokens.verify_at(Some(&token), "c1", now + 11),
            Err(TokenRefusal::Reused)
        );
        let late = tokens.mint_at("c1", now);
        assert_eq!(
            tokens.verify_at(Some(&late), "c1", now + 61),
            Err(TokenRefusal::Expired)
        );
        // Another deployment's key, a tampered expiry, garbage, nothing.
        let other = BridgeTokens::new("other-secret", 60).mint_at("c1", now);
        assert_eq!(
            tokens.verify_at(Some(&other), "c1", now),
            Err(TokenRefusal::Forged)
        );
        let fresh = tokens.mint_at("c1", now);
        let mut parts: Vec<&str> = fresh.split('.').collect();
        let extended = (now + 100_000).to_string();
        parts[1] = &extended;
        assert_eq!(
            tokens.verify_at(Some(&parts.join(".")), "c1", now),
            Err(TokenRefusal::Forged)
        );
        assert_eq!(
            tokens.verify_at(Some("media-secret"), "c1", now),
            Err(TokenRefusal::Malformed),
            "the static media token is not a bridge token"
        );
        assert_eq!(
            tokens.verify_at(None, "c1", now),
            Err(TokenRefusal::Missing)
        );
    }

    #[test]
    fn urls_are_logged_without_their_query() {
        assert_eq!(
            redact_url("ws://engine:8080/voip/bridge?token=v1.x&call_id=c1"),
            "ws://engine:8080/voip/bridge?…"
        );
        assert_eq!(redact_url("ws://engine:8080/voip"), "ws://engine:8080/voip");
        assert_eq!(
            redact_tokens(
                "transfer to voip_bridge:ws://e/voip/bridge?token=v1.9.ab&call_id=c failed"
            ),
            "transfer to voip_bridge:ws://e/voip/bridge?token=…&call_id=c failed"
        );
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(None), None);
    }
}
