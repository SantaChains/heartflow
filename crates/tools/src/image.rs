use std::env;
use std::io::{Cursor, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader};
use runtime::ContentBlock;
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

/// Media types accepted for a user attachment, keyed by file extension.
const ATTACHMENT_MEDIA_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];
/// Attachment cap. The transcript keeps the encoded bytes, so stay far below
/// the 48 MiB request ceiling every gateway shares.
const MAX_ATTACHMENT_BYTES: u64 = 4 * 1024 * 1024;
/// Longest edge a vision encoder actually keeps. Providers resize anything
/// larger themselves (Claude tops out near 1568 px), so pixels past this point
/// only buy upload bytes, per-turn transcript rewriting, memory, and disk for
/// detail the model never sees.
const VISION_MAX_EDGE: u32 = 1568;
/// JPEG quality for the reshaped path. The original is kept whenever shrinking
/// fails to produce fewer bytes, so reshaping can never inflate an attachment.
const JPEG_QUALITY: u8 = 85;

/// Media type for a file extension providers accept, `None` otherwise.
#[must_use]
pub fn attachment_media_type(path: &str) -> Option<&'static str> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)?
        .to_ascii_lowercase();
    ATTACHMENT_MEDIA_TYPES
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, media_type)| *media_type)
}

/// Read a local image into an inline content block, base64 encoded as every
/// dialect expects it on the wire.
///
/// The bytes are reshaped to the vision grid first (see
/// [`shrink_to_vision_grid`]): a transcript is rewritten, mirrored, and
/// re-uploaded on every single turn, so an oversized photo would cost its full
/// size per turn for pixels the provider discards anyway.
pub fn read_image_attachment(path: &str) -> Result<ContentBlock, ImageError> {
    let media_type = attachment_media_type(path)
        .ok_or_else(|| ImageError::Invalid(format!("unsupported image type: {path}")))?;
    let metadata = std::fs::metadata(path).map_err(|error| ImageError::Io(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ImageError::Io(format!("not a file: {path}")));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(ImageError::Invalid(format!(
            "{path} exceeds {} MiB",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| ImageError::Io(error.to_string()))?;
    let (media_type, bytes) = match shrink_to_vision_grid(media_type, &bytes) {
        Some((media_type, reshaped)) => (media_type, reshaped),
        None => (media_type.to_string(), bytes),
    };
    Ok(ContentBlock::Image {
        media_type,
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

/// Reshape an attachment down to what a vision encoder can use, or `None` when
/// the bytes are already fine and must travel verbatim.
///
/// Only the two formats a camera or a screenshot tool produces are touched, and
/// only when a cheap header probe says the long edge is over the grid. Those are
/// by far the bulky attachments; gif and webp may be animated, and re-encoding a
/// single decoded frame would silently drop the animation. A file that will not
/// decode is passed through too: refusing it here would break formats the
/// provider itself accepts.
fn shrink_to_vision_grid(media_type: &str, bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let format = match media_type {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        _ => return None,
    };
    let size = imagesize::blob_size(bytes).ok()?;
    if size.width.max(size.height) <= VISION_MAX_EDGE as usize {
        return None;
    }
    let scaled = decode_oriented(bytes)?.resize(
        VISION_MAX_EDGE,
        VISION_MAX_EDGE,
        image::imageops::FilterType::Lanczos3,
    );
    let reshaped = encode(&scaled, format)?;
    // Keep whichever encoding is smaller: a re-encode of an already efficient
    // file can grow, and the grid cap must never cost more bytes than it saves.
    if reshaped.len() >= bytes.len() {
        return None;
    }
    Some((media_type.to_string(), reshaped))
}

/// Decode an attachment and apply its EXIF orientation, so a phone photo taken
/// in portrait does not reach the model lying on its side.
fn decode_oriented(bytes: &[u8]) -> Option<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

/// Re-encode in the attachment's own format, which keeps PNG lossless (and its
/// alpha) and JPEG photographic rather than converting between the two.
fn encode(image: &DynamicImage, format: ImageFormat) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
                .encode_image(&image.to_rgb8())
                .ok()?;
        }
        ImageFormat::Png => image
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .ok()?,
        _ => return None,
    }
    Some(out)
}

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
    use super::{
        build_request_body, read_image_attachment, resolve_output_path, BytesSource, ImageConfig,
        VISION_MAX_EDGE,
    };
    use crate::image::{source_from_response, ImageResponse};
    use base64::Engine as _;
    use runtime::ContentBlock;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    /// A smooth gradient: compressible, so a wide test image stays well under the
    /// attachment cap while still exceeding the vision grid.
    fn gradient(width: u32, height: u32) -> image::RgbImage {
        let mut image = image::RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        image
    }

    fn temp_path(tag: &str, extension: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("tools-image-{tag}-{nanos}.{extension}"))
    }

    fn attach(pixels: &image::RgbImage, tag: &str, format: image::ImageFormat) -> ContentBlock {
        let extension = format.extensions_str()[0];
        let path = temp_path(tag, extension);
        pixels
            .save_with_format(&path, format)
            .expect("test image should save");
        let block = read_image_attachment(path.to_str().expect("utf8 temp path"))
            .expect("attachment should read");
        let _ = std::fs::remove_file(&path);
        block
    }

    fn image_payload(block: &ContentBlock) -> (String, Vec<u8>) {
        match block {
            ContentBlock::Image { media_type, data } => (
                media_type.clone(),
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("payload should be base64"),
            ),
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[test]
    fn shrinks_oversized_png_to_the_vision_grid() {
        let block = attach(
            &gradient(2400, 1200),
            "oversized-png",
            image::ImageFormat::Png,
        );
        let (media_type, bytes) = image_payload(&block);
        assert_eq!(media_type, "image/png");

        let size = imagesize::blob_size(&bytes).expect("reshaped size");
        assert_eq!(size.width, VISION_MAX_EDGE as usize);
        assert_eq!(size.height, (VISION_MAX_EDGE / 2) as usize);
    }

    #[test]
    fn shrinks_oversized_jpeg_to_the_vision_grid() {
        let block = attach(
            &gradient(3000, 1000),
            "oversized-jpeg",
            image::ImageFormat::Jpeg,
        );
        let (media_type, bytes) = image_payload(&block);
        assert_eq!(media_type, "image/jpeg");

        let size = imagesize::blob_size(&bytes).expect("reshaped size");
        assert_eq!(size.width, VISION_MAX_EDGE as usize);
        assert!(
            bytes.len() < 200 * 1024,
            "reshaped jpeg stayed large: {}",
            bytes.len()
        );
    }

    #[test]
    fn keeps_an_attachment_inside_the_grid_byte_identical() {
        let path = temp_path("small-png", "png");
        gradient(320, 200)
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("test image should save");
        let raw = std::fs::read(&path).expect("read back");
        let block = read_image_attachment(path.to_str().expect("utf8 temp path")).expect("read");
        let _ = std::fs::remove_file(&path);

        let (_, bytes) = image_payload(&block);
        assert_eq!(bytes, raw);
    }

    #[test]
    fn passes_non_photographic_formats_through_untouched() {
        // gif and webp may be animated, so they must never be re-encoded frame by
        // frame; the bytes are handed over exactly as they are on disk.
        let path = temp_path("pass-through", "gif");
        let raw = b"GIF89a-not-actually-decoded".to_vec();
        std::fs::write(&path, &raw).expect("write raw bytes");
        let block = read_image_attachment(path.to_str().expect("utf8 temp path")).expect("read");
        let _ = std::fs::remove_file(&path);

        let (media_type, bytes) = image_payload(&block);
        assert_eq!(media_type, "image/gif");
        assert_eq!(bytes, raw);
    }
}
