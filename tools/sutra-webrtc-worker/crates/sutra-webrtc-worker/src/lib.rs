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
