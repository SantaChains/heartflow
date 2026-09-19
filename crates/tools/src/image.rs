use std::env;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::web::ensure_public;

/// Base URL must include the version segment; OpenAI-compatible image gateways
/// (also many domestic relays) expose `POST {base}/images/generations`.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-image-1";
const DEFAULT_SIZE: &str = "1024x1024";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Images are a few MB; cap well above that but bounded.
const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const USER_AGENT: &str = "heartflow/1.0";

/// Credentials and endpoint are opt-in via environment so this never shadows the
/// chat provider's key. `HEARTFLOW_IMAGE_API_KEY` is required at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    pub base_url: String,
    pub model: String,
    pub size: String,
    pub api_key: Option<String>,
}

impl ImageConfig {
    /// Merge environment defaults with a per-call model/size override.
    #[must_use]
    pub fn from_env(model_override: Option<&str>, size_override: Option<&str>) -> Self {
        Self {
            base_url: env::var("HEARTFLOW_IMAGE_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            model: model_override
                .map(str::to_string)
                .or_else(|| env::var("HEARTFLOW_IMAGE_MODEL").ok())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            size: size_override
                .map(str::to_string)
                .or_else(|| env::var("HEARTFLOW_IMAGE_SIZE").ok())
                .unwrap_or_else(|| DEFAULT_SIZE.to_string()),
            api_key: env::var("HEARTFLOW_IMAGE_API_KEY").ok(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct GenerateImageInput {
    pub prompt: String,
    pub model: Option<String>,
    pub size: Option<String>,
    /// Where to write the produced file; defaults to a timestamped `.png` in cwd.
    pub output_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GenerateImageReport {
    pub path: String,
    pub bytes: usize,
    pub model: String,
    pub size: String,
}

#[derive(Debug)]
pub enum ImageError {
    NotConfigured(String),
    Invalid(String),
    Http(String),
    NoImage,
    Io(String),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(msg) => write!(f, "image tool not configured: {msg}"),
            Self::Invalid(msg) => write!(f, "invalid request: {msg}"),
            Self::Http(msg) => write!(f, "image request failed: {msg}"),
            Self::NoImage => write!(f, "provider returned no image data"),
            Self::Io(msg) => write!(f, "file error: {msg}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Wire shape of an OpenAI-compatible image response. Providers return either
/// base64 (`b64_json`) or a hosted `url`; both are handled.
#[derive(Debug, Deserialize)]
struct ImageResponse {
    data: Option<Vec<ImageDatum>>,
    error: Option<ProviderError>,
}

#[derive(Debug, Deserialize)]
struct ImageDatum {
    b64_json: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderError {
    message: Option<String>,
}

/// Build the JSON request body. Deliberately omits `response_format` so the same
/// call works across `gpt-image-1` (always base64) and DALL·E-style gateways.
#[must_use]
pub fn build_request_body(config: &ImageConfig, prompt: &str) -> serde_json::Value {
    json!({
        "model": config.model,
        "prompt": prompt,
        "n": 1,
        "size": config.size,
    })
}

/// Decide where the raw image bytes come from: inline base64 wins, else a hosted
/// URL (validated http/https and public).
fn source_from_response(response: &ImageResponse) -> Result<BytesSource, ImageError> {
    if let Some(error) = &response.error {
        let message = error
            .message
            .clone()
            .unwrap_or_else(|| "provider reported an error".to_string());
        return Err(ImageError::Http(message));
    }
    let datum = response
        .data
        .as_ref()
        .and_then(|items| items.first())
        .ok_or(ImageError::NoImage)?;
    if let Some(b64) = &datum.b64_json {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|error| ImageError::Invalid(format!("base64 decode: {error}")))?;
        return Ok(BytesSource::Bytes(bytes));
    }
    if let Some(url) = &datum.url {
        return Ok(BytesSource::Url(url.clone()));
    }
    Err(ImageError::NoImage)
}

enum BytesSource {
    Bytes(Vec<u8>),
    Url(String),
}

/// Choose the output path: honor the caller's, else `generated_<millis>.png` in
/// the current directory.
#[must_use]
pub fn resolve_output_path(output_path: Option<&str>) -> String {
    match output_path {
        Some(path) if !path.trim().is_empty() => path.to_string(),
        _ => {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_millis())
                .unwrap_or_default();
            format!("generated_{millis}.png")
        }
    }
}

/// Generate an image with an OpenAI-compatible `/images/generations` endpoint and
/// write the bytes to disk. Network + credentials required; guarded against SSRF
/// when the provider returns a hosted URL.
pub fn generate_image(input: &GenerateImageInput) -> Result<GenerateImageReport, ImageError> {
    let config = ImageConfig::from_env(input.model.as_deref(), input.size.as_deref());
    if input.prompt.trim().is_empty() {
        return Err(ImageError::Invalid("prompt is empty".to_string()));
    }
    let api_key = config
        .api_key
        .clone()
        .ok_or_else(|| ImageError::NotConfigured("set HEARTFLOW_IMAGE_API_KEY".to_string()))?;

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| ImageError::Http(error.to_string()))?;
    let endpoint = format!(
        "{}/images/generations",
        config.base_url.trim_end_matches('/')
    );
    let response = client
        .post(&endpoint)
        .bearer_auth(&api_key)
        .json(&build_request_body(&config, &input.prompt))
        .send()
        .map_err(|error| ImageError::Http(error.to_string()))?;
    let status = response.status();
    let body: ImageResponse = response.json().map_err(|error| {
        ImageError::Http(format!("status {status}, bad response body: {error}"))
    })?;

    let bytes = match source_from_response(&body)? {
        BytesSource::Bytes(bytes) => bytes,
        BytesSource::Url(url) => {
            let parsed = reqwest::Url::parse(&url)
                .map_err(|error| ImageError::Invalid(error.to_string()))?;
            ensure_public(&parsed).map_err(|error| ImageError::Invalid(error.to_string()))?;
            let fetched = client
                .get(parsed)
                .send()
                .map_err(|error| ImageError::Http(error.to_string()))?;
            let mut limited = fetched.take(MAX_IMAGE_BYTES + 1);
            let mut buf = Vec::new();
            limited
                .read_to_end(&mut buf)
                .map_err(|error| ImageError::Http(error.to_string()))?;
            if buf.len() as u64 > MAX_IMAGE_BYTES {
                return Err(ImageError::Invalid("image exceeds size cap".to_string()));
            }
            buf
        }
    };

    let path = resolve_output_path(input.output_path.as_deref());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| ImageError::Io(error.to_string()))?;
        }
    }
    std::fs::write(&path, &bytes).map_err(|error| ImageError::Io(error.to_string()))?;
    Ok(GenerateImageReport {
        path,
        bytes: bytes.len(),
        model: config.model,
        size: config.size,
    })
}

#[cfg(test)]
mod tests {
    use super::{build_request_body, resolve_output_path, BytesSource, ImageConfig};
    use crate::image::{source_from_response, ImageResponse};
    use serde_json::json;

    fn config() -> ImageConfig {
        ImageConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-image-1".to_string(),
            size: "1024x1024".to_string(),
            api_key: Some("k".to_string()),
        }
    }

    #[test]
    fn request_body_omits_response_format() {
        let body = build_request_body(&config(), "a red cube");
        assert_eq!(body["model"], "gpt-image-1");
        assert_eq!(body["prompt"], "a red cube");
        assert_eq!(body["n"], 1);
        assert_eq!(body["size"], "1024x1024");
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn decodes_inline_base64() {
        let response: ImageResponse =
            serde_json::from_value(json!({ "data": [{ "b64_json": "aGVsbG8=" }] })).expect("parse");
        match source_from_response(&response).expect("source") {
            BytesSource::Bytes(bytes) => assert_eq!(bytes, b"hello"),
            BytesSource::Url(_) => panic!("expected inline bytes"),
        }
    }

    #[test]
    fn falls_back_to_url_and_surfaces_provider_error() {
        let response: ImageResponse =
            serde_json::from_value(json!({ "data": [{ "url": "https://cdn/x.png" }] }))
                .expect("parse");
        assert!(matches!(
            source_from_response(&response).expect("source"),
            BytesSource::Url(url) if url == "https://cdn/x.png"
        ));

        let err: ImageResponse =
            serde_json::from_value(json!({ "error": { "message": "no quota" } })).expect("parse");
        assert!(source_from_response(&err).is_err());

        let empty: ImageResponse = serde_json::from_value(json!({ "data": [] })).expect("parse");
        assert!(source_from_response(&empty).is_err());
    }

    #[test]
    fn output_path_honors_override_or_defaults_to_png() {
        assert_eq!(
            resolve_output_path(Some("out/art.png")),
            "out/art.png".to_string()
        );
        let generated = resolve_output_path(None);
        assert!(generated.starts_with("generated_"));
        assert!(generated.contains(".png"));
        assert!(resolve_output_path(Some("   ")).starts_with("generated_"));
    }
}
