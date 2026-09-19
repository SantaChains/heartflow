//! Graphic-file verification for model-generated output.
//!
//! Text-only models (`DeepSeek` included) cannot see the files they write, so
//! every graphic artifact gets a structural check here: real images are
//! validated by magic bytes plus real dimensions, SVG by strict XML parsing
//! with an `svg` root element, and HTML by tag balance.

use std::fs;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;

const MAX_VERIFY_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct GraphicsReport {
    pub path: String,
    pub format: String,
    pub valid: bool,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<String>,
    pub details: String,
}

#[derive(Debug)]
pub struct GraphicsError(String);

impl std::fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn verify_graphics(path: &str) -> Result<GraphicsReport, GraphicsError> {
    let metadata = fs::metadata(path)
        .map_err(|error| GraphicsError(format!("cannot stat {path}: {error}")))?;
    if metadata.len() > MAX_VERIFY_BYTES {
        return Err(GraphicsError(format!(
            "{path} is {} bytes; verification is limited to {MAX_VERIFY_BYTES}",
            metadata.len()
        )));
    }
    let bytes =
        fs::read(path).map_err(|error| GraphicsError(format!("cannot read {path}: {error}")))?;
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut report = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => verify_bitmap(&extension, &bytes),
        "svg" => verify_svg(&bytes),
        "html" | "htm" => verify_html(&bytes),
        other => {
            return Err(GraphicsError(format!(
                "unsupported graphic format: .{other} (use png/jpg/gif/webp/bmp/svg/html)"
            )));
        }
    };
    report.path = path.to_string();
    report.size_bytes = metadata.len();
    if report.valid && !matches!(extension.as_str(), "html" | "htm") {
        report.dimensions = image_dimensions(path);
    }
    Ok(report)
}

fn verify_bitmap(extension: &str, bytes: &[u8]) -> GraphicsReport {
    let valid = match extension {
        "png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "jpg" | "jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "bmp" => bytes.starts_with(b"BM"),
        _ => false,
    };
    let details = if valid {
        format!("{extension} signature OK")
    } else {
        format!(
            "file does not match the {extension} magic signature; likely corrupt or fake binary"
        )
    };
    GraphicsReport {
        path: String::new(),
        format: extension.to_string(),
        valid,
        size_bytes: 0,
        dimensions: None,
        details,
    }
}

fn verify_svg(bytes: &[u8]) -> GraphicsReport {
    let text = std::str::from_utf8(bytes).map_err(|_| "svg is not valid UTF-8");
    let text = match text {
        Ok(text) => text,
        Err(message) => {
            return GraphicsReport {
                path: String::new(),
                format: "svg".to_string(),
                valid: false,
                size_bytes: 0,
                dimensions: None,
                details: message.to_string(),
            };
        }
    };

    let mut reader = Reader::from_str(text);
    let mut root_seen = false;
    let mut root_is_svg = false;
    let mut elements = 0_usize;
    let mut failure: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(event)) => {
                let qname = event.name();
                let name = qname.as_ref();
                if !root_seen {
                    root_seen = true;
                    root_is_svg = name == "svg";
                }
                elements += 1;
            }
            Ok(Event::Empty(_)) => elements += 1,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                failure = Some(format!("XML parse error: {error}"));
                break;
            }
        }
        buf.clear();
    }

    let (valid, details) = if let Some(message) = failure {
        (false, message)
    } else if !root_seen {
        (false, "no root element found".to_string())
    } else if !root_is_svg {
        (false, "root element is not <svg>".to_string())
    } else {
        (true, format!("well-formed XML, {elements} elements"))
    };
    GraphicsReport {
        path: String::new(),
        format: "svg".to_string(),
        valid,
        size_bytes: 0,
        dimensions: None,
        details,
    }
}

/// Balanced-tag check for HTML. Void elements never push; the stack must end
/// empty for the document to be well-formed.
fn verify_html(bytes: &[u8]) -> GraphicsReport {
    const VOID: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source",
        "track", "wbr",
    ];
    let Ok(text) = std::str::from_utf8(bytes) else {
        return GraphicsReport {
            path: String::new(),
            format: "html".to_string(),
            valid: false,
            size_bytes: 0,
            dimensions: None,
            details: "html is not valid UTF-8".to_string(),
        };
    };

    let mut stack: Vec<String> = Vec::new();
    let mut bytes_iter = text.char_indices().peekable();
    while let Some((index, ch)) = bytes_iter.next() {
        if ch != '<' {
            continue;
        }
        let Some((close_offset, close_ch)) = bytes_iter.peek().copied() else {
            break;
        };
        let closing = close_ch == '/';
        let start = if closing {
            close_offset + 1
        } else {
            close_offset
        };
        let mut name = String::new();
        for (_, c) in text[start..].char_indices() {
            if c.is_ascii_alphanumeric() || c == '-' {
                name.push(c.to_ascii_lowercase());
            } else {
                break;
            }
        }
        if name.is_empty() {
            continue;
        }
        if closing {
            match stack.pop() {
                Some(open) if open == name => {}
                Some(open) => {
                    stack.clear();
                    return simple_html_report(
                        false,
                        format!("mismatched tag: <{open}> closed by </{name}> at byte {index}"),
                    );
                }
                None => {
                    return simple_html_report(false, format!("stray </{name}> at byte {index}"));
                }
            }
        } else if !VOID.contains(&name.as_str()) {
            stack.push(name);
        }
    }

    if stack.is_empty() {
        simple_html_report(true, "all tags balanced".to_string())
    } else {
        simple_html_report(false, format!("unclosed tags: {}", stack.join(", ")))
    }
}

fn simple_html_report(valid: bool, details: String) -> GraphicsReport {
    GraphicsReport {
        path: String::new(),
        format: "html".to_string(),
        valid,
        size_bytes: 0,
        dimensions: None,
        details,
    }
}

fn image_dimensions(path: &str) -> Option<String> {
    imagesize::size(path)
        .ok()
        .map(|size| format!("{}x{}", size.width, size.height))
}

#[cfg(test)]
mod tests {
    use super::verify_graphics;
    use std::fs;
    use std::io::Write;

    fn temp_file(name: &str, contents: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!("hf-graphics-{name}"));
        let mut file = fs::File::create(&path).expect("create temp file");
        file.write_all(contents).expect("write temp file");
        path.display().to_string()
    }

    #[test]
    fn accepts_real_png_signature() {
        let bytes = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        let report = verify_graphics(&temp_file("ok.png", &bytes)).expect("verify");
        assert!(report.valid);
        assert_eq!(report.format, "png");
    }

    #[test]
    fn rejects_fake_png() {
        let report = verify_graphics(&temp_file("fake.png", b"<svg></svg>")).expect("verify");
        assert!(!report.valid);
        assert!(report.details.contains("magic signature"));
    }

    #[test]
    fn validates_well_formed_svg() {
        let report = verify_graphics(&temp_file(
            "good.svg",
            br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#,
        ))
        .expect("verify");
        assert!(report.valid);
        assert!(report.details.contains("well-formed"));
    }

    #[test]
    fn rejects_broken_svg() {
        let report = verify_graphics(&temp_file("bad.svg", b"<svg><rect></svg>")).expect("verify");
        assert!(!report.valid);
    }

    #[test]
    fn validates_balanced_html_and_flags_mismatch() {
        let good = verify_graphics(&temp_file(
            "good.html",
            b"<html><body><p>hi<br/></p></body></html>",
        ))
        .expect("verify");
        assert!(good.valid);

        let bad = verify_graphics(&temp_file(
            "bad.html",
            b"<html><body><p>hi</body></p></html>",
        ))
        .expect("verify");
        assert!(!bad.valid);
        assert!(bad.details.contains("mismatched"));
    }

    #[test]
    fn errors_on_unknown_format() {
        let error = verify_graphics(&temp_file("x.txt", b"hello")).expect_err("unsupported");
        assert!(error.to_string().contains("unsupported"));
    }
}
