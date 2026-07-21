use super::{PreparedImage, ToolExecError, context::ToolContext, truncate_tool_output};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::GenericImageView;
use resvg::usvg;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;
use std::{io, time::Duration};
use tai_keystore::ServiceCredential;
use url::Url;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DisplayImageArgs {
    /// MIME type of the image (e.g. "image/png", "image/svg+xml")
    mime_type: String,
    /// Path to an image file on disk
    path: Option<String>,
    /// URL of an image to fetch and display
    url: Option<String>,
    /// Base64-encoded image data
    base64_data: Option<String>,
    /// Raw SVG markup to render
    svg_text: Option<String>,
    /// Alt text description of the image
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
                "image exceeds maximum allowed size of {}",
                humfmt::bytes(MAX_DISPLAY_IMAGE_BYTES as u64),
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

fn fetch_image_bytes(url_str: &str, expected_mime_type: &str) -> io::Result<Vec<u8>> {
    let url =
        Url::parse(url_str).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "image url must use http or https",
            ));
        }
    }

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(IMAGE_FETCH_TIMEOUT_SECS)))
            .http_status_as_error(false)
            .build(),
    );
    let response = agent.get(url.as_str()).call().map_err(io::Error::other)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(io::Error::other(format!(
            "image request failed with status {status}"
        )));
    }
    if let Some(content_type) = response.headers().get("content-type")
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
    let bytes = response
        .into_body()
        .read_to_vec()
        .map_err(io::Error::other)?;
    Ok(bytes)
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
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "display_image"
    }
    fn description(&self) -> &'static str {
        "Display a PNG, JPEG, or SVG image in the client UI."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Displaying image ({}).", args.mime_type)];
        if let Some(ref p) = args.path {
            parts.push(format!(" Path: `{}`.", p));
        }
        if let Some(ref u) = args.url {
            parts.push(format!(" URL: {}.", u));
        }
        if args.base64_data.is_some() {
            parts.push(" Source: base64 data.".to_string());
        }
        if args.svg_text.is_some() {
            parts.push(" Source: SVG markup.".to_string());
        }
        if let Some(ref alt) = args.alt {
            parts.push(format!(" Alt text: {}.", alt));
        }
        parts.concat()
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let image = prepare_image(&args).map_err(|e| ToolExecError(e.to_string()))?;
        let mime_type = image.mime_type.clone();
        let width = image.width;
        let height = image.height;
        let byte_len = image.data.len();
        *self.last_image.lock().unwrap_or_else(|e| e.into_inner()) = Some(image);
        Ok(truncate_tool_output(&format!(
            "displayed image ({mime_type}, {width}x{height}, {})",
            humfmt::bytes(byte_len as u64),
        )))
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        self.last_image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
