use super::{PreparedImage, ToolError, context::ToolContext, truncate_tool_output};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::GenericImageView;
use reqwest::{Url, header::CONTENT_TYPE};
use resvg::usvg;
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;
use std::{io, time::Duration};
use tai_keystore::ServiceCredential;

#[derive(Debug, Deserialize)]
pub struct DisplayImageArgs {
    mime_type: String,
    path: Option<String>,
    url: Option<String>,
    base64_data: Option<String>,
    svg_text: Option<String>,
    alt: Option<String>,
}

const MAX_DISPLAY_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/svg+xml"];
const IMAGE_FETCH_TIMEOUT_SECS: u64 = 10;

fn prepare_image(args: &DisplayImageArgs) -> io::Result<PreparedImage> {
    let mime_type = normalize_image_mime_type(&args.mime_type)?;
    let selected_sources = [
        args.path.as_ref().map(|_| "path"),
        args.url.as_ref().map(|_| "url"),
        args.base64_data.as_ref().map(|_| "base64_data"),
        args.svg_text.as_ref().map(|_| "svg_text"),
    ]
    .into_iter()
    .flatten()
    .count();
    if selected_sources != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provide exactly one image source: path, url, base64_data, or svg_text",
        ));
    }

    let data = if let Some(path) = &args.path {
        std::fs::read(path.trim())?
    } else if let Some(url) = &args.url {
        fetch_image_bytes(url.trim(), mime_type)?
    } else if let Some(base64_data) = &args.base64_data {
        BASE64.decode(base64_data.trim()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid base64_data: {error}"),
            )
        })?
    } else if let Some(svg_text) = &args.svg_text {
        svg_text.as_bytes().to_vec()
    } else {
        unreachable!("source count validated")
    };

    if data.len() > MAX_DISPLAY_IMAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "image exceeds maximum allowed size of {} bytes",
                MAX_DISPLAY_IMAGE_BYTES
            ),
        ));
    }

    let (width, height) = inspect_image_dimensions(mime_type, &data)?;
    Ok(PreparedImage {
        mime_type: mime_type.to_string(),
        data,
        width,
        height,
        alt: args.alt.clone().filter(|alt| !alt.trim().is_empty()),
    })
}

fn normalize_image_mime_type(mime_type: &str) -> io::Result<&str> {
    let normalized = mime_type.trim();
    if SUPPORTED_IMAGE_MIME_TYPES.contains(&normalized) {
        Ok(normalized)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported image mime type: {normalized}"),
        ))
    }
}

fn fetch_image_bytes(url: &str, expected_mime_type: &str) -> io::Result<Vec<u8>> {
    let url =
        Url::parse(url).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "image url must use http or https",
            ));
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(IMAGE_FETCH_TIMEOUT_SECS))
        .build()
        .map_err(io::Error::other)?;
    let response = client.get(url).send().map_err(io::Error::other)?;
    let status = response.status();
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "image request failed with status {status}"
        )));
    }
    if let Some(content_type) = response.headers().get(CONTENT_TYPE)
        && let Ok(content_type) = content_type.to_str()
        && !content_type.starts_with(expected_mime_type)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "image response content-type {content_type} does not match {expected_mime_type}"
            ),
        ));
    }
    let bytes = response.bytes().map_err(io::Error::other)?;
    Ok(bytes.to_vec())
}

fn inspect_image_dimensions(mime_type: &str, data: &[u8]) -> io::Result<(u32, u32)> {
    match mime_type {
        "image/png" | "image/jpeg" => {
            let image = image::load_from_memory(data).map_err(io::Error::other)?;
            Ok(image.dimensions())
        }
        "image/svg+xml" => {
            let options = usvg::Options::default();
            let tree = usvg::Tree::from_data(data, &options).map_err(io::Error::other)?;
            let size = tree.size().to_int_size();
            Ok((size.width(), size.height()))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported image mime type: {mime_type}"),
        )),
    }
}

pub(crate) struct DisplayImage {
    last_image: Mutex<Option<PreparedImage>>,
}

impl DisplayImage {
    pub(crate) fn new() -> Self {
        DisplayImage {
            last_image: Mutex::new(None),
        }
    }
}

impl super::Tool for DisplayImage {
    type Args = DisplayImageArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "display_image"
    }
    fn description(&self) -> &'static str {
        "Display a PNG, JPEG, or SVG image in the client UI."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mime_type": {
                    "type": "string",
                    "enum": ["image/png", "image/jpeg", "image/svg+xml"]
                },
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path to an image file"
                },
                "url": {
                    "type": "string",
                    "description": "Absolute http or https URL to an image"
                },
                "base64_data": {
                    "type": "string",
                    "description": "Raw image bytes encoded as base64"
                },
                "svg_text": {
                    "type": "string",
                    "description": "Inline SVG document text; only valid with mime_type image/svg+xml"
                },
                "alt": {
                    "type": "string",
                    "description": "Optional accessible description"
                }
            },
            "required": ["mime_type"],
            "additionalProperties": false
        })
    }
    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let image = prepare_image(&args).map_err(|e| ToolError::Other(e.to_string()))?;
        let mime_type = image.mime_type.clone();
        let width = image.width;
        let height = image.height;
        let byte_len = image.data.len();
        *self.last_image.lock().unwrap_or_else(|e| e.into_inner()) = Some(image);
        Ok(truncate_tool_output(&format!(
            "displayed image ({mime_type}, {width}x{height}, {byte_len} bytes)"
        )))
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        self.last_image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
