//! What the worker and the scripted participant share: settings from the environment (secrets
//! as references only) and LiveKit access-token minting.

use std::time::Duration;

use livekit_api::access_token::{AccessToken, VideoGrants};
use sutra_webrtc_core::secret;

/// A setting from the environment, trimmed; `None` when unset or empty.
pub fn setting(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A required setting.
pub fn required(name: &str) -> Result<String, String> {
    setting(name).ok_or_else(|| format!("{name} is required"))
}

/// A secret setting: the variable holds a reference (`env:NAME`, `file:/path`), never the
/// secret itself.
pub fn secret_setting(name: &str) -> Result<String, String> {
    let reference = required(name)?;
    secret::resolve(name, &reference, |var| std::env::var(var).ok())
}

/// A numeric setting with a default.
pub fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match setting(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<T>()
            .map_err(|_| format!("{name}: '{raw}' is not a number")),
    }
}

/// A boolean setting with a default.
pub fn flag(name: &str, default: bool) -> Result<bool, String> {
    match setting(name).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if v == "true" || v == "1" || v == "yes" => Ok(true),
        Some(v) if v == "false" || v == "0" || v == "no" => Ok(false),
        Some(v) => Err(format!("{name}: '{v}' is not true or false")),
    }
}

/// The engine's media-stream request: `url` (`SUTRA_WORKER_ENGINE_URL`) with the media token
/// as an `Authorization: Bearer` header — never in the URL, which proxies and servers log. A
/// URL that carries a `token` itself is refused.
pub fn engine_request(
    url: &str,
    media_token: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    if url
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|p| p.starts_with("token=")))
    {
        return Err(
            "SUTRA_WORKER_ENGINE_URL carries a token; the media token goes in SUTRA_WORKER_MEDIA_TOKEN and is sent as a header"
                .to_string(),
        );
    }
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("SUTRA_WORKER_ENGINE_URL: {e}"))?;
    let value = format!("Bearer {media_token}")
        .parse()
        .map_err(|_| "the media token is not a valid header value".to_string())?;
    request.headers_mut().insert("authorization", value);
    Ok(request)
}

/// What a token grants.
pub struct TokenRequest<'a> {
    pub api_key: &'a str,
    pub api_secret: &'a str,
    pub room: &'a str,
    pub identity: &'a str,
    pub name: &'a str,
    pub attributes: Vec<(String, String)>,
    pub can_subscribe: bool,
    pub can_publish: bool,
}

/// Mint a room-join token (6 hours).
pub fn mint_token(request: &TokenRequest<'_>) -> Result<String, String> {
    AccessToken::with_api_key(request.api_key, request.api_secret)
        .with_identity(request.identity)
        .with_name(request.name)
        .with_attributes(request.attributes.clone())
        .with_ttl(Duration::from_secs(6 * 3600))
        .with_grants(VideoGrants {
            room_join: true,
            room: request.room.to_string(),
            can_subscribe: Some(request.can_subscribe),
            can_publish: Some(request.can_publish),
            can_publish_data: Some(false),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| format!("token: {e}"))
}

/// Install the log subscriber (`RUST_LOG`, default `info`, quieter for the SDK's internals).
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info,libwebrtc=warn,livekit=warn,livekit_api=warn")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_media_token_rides_in_a_header_not_the_url() {
        let request = engine_request("ws://engine:8080/voip/stream", "s3cret").unwrap();
        assert_eq!(request.uri().to_string(), "ws://engine:8080/voip/stream");
        assert_eq!(request.headers()["authorization"], "Bearer s3cret");
        assert!(!request.uri().to_string().contains("s3cret"));
        assert!(engine_request("ws://engine:8080/voip/stream?token=s3cret", "s3cret").is_err());
        assert!(engine_request("ws://engine:8080/voip/stream", "bad\nvalue").is_err());
    }
}
