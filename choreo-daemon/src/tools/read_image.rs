//! `read_image`: read an image file from disk and feed it back to a
//! vision-capable model.
//!
//! The tool normalizes the image (resize / MIME / re-encode — see
//! `crate::image_prep`), reports a text handle (path, dimensions, MIME, byte
//! size) the model can reason about, and carries an [`ImageReference`] (path +
//! metadata + the normalized bytes) in its `Return` value that the framework
//! hands to the request builder via the `extract_image_ref` hook. The reference
//! now carries the normalized bytes durably (daemon-only), so the request
//! builder attaches them directly instead of re-reading the source file.
//!
//! The reference travels with the per-invocation `ReadImageReturn` (read from
//! `ret` in the hook), not a shared `Mutex` slot: the tool is registered once
//! and shared across sessions, so parking the reference on `&self` would let a
//! concurrent session's invocation overwrite it before this invocation's hook
//! reads it back.
//!
//! v1 accepts local file paths only. URLs / clipboard paste are future work.

use super::{ToolExecError, truncate_tool_output};
use crate::image_prep;
use choreo_keystore::ServiceCredential;
use choreo_proto::ImageReference;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{debug, info, warn};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadImageArgs {
    /// Path to an image file to read (relative to the working directory, or
    /// absolute).
    pub path: String,
}

/// The `read_image` tool's return value: a human-readable text handle plus the
/// parsed image reference, so the framework's `extract_image_ref` hook reads
/// the reference straight off the per-invocation return value (no shared
/// state). `impl Serialize` emits only `text`, so the JSON tool result is a
/// plain string exactly as before.
#[derive(Debug)]
pub struct ReadImageReturn {
    /// The text handle (path, dimensions, MIME, bytes) shown to the model.
    pub text: String,
    /// The image reference (path + metadata) fed back to vision models.
    pub reference: ImageReference,
}

impl Serialize for ReadImageReturn {
    /// Serialize to just the text handle, keeping the JSON wire format a plain
    /// string (identical to the previous `Return = String`).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl JsonSchema for ReadImageReturn {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ReadImageReturn")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

pub(crate) struct ReadImage {}

impl ReadImage {
    pub(crate) fn new() -> Self {
        ReadImage {}
    }
}

/// Resolve `path` against the working directory and normalize the file into a
/// vision image. Returns the prepared image plus the resolved path to embed in
/// the reference.
fn resolve_and_normalize(
    args: &ReadImageArgs,
    working_dir: Option<&Path>,
) -> Result<(std::path::PathBuf, image_prep::PreparedVisionImage), ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = super::resolve_path(&args.path, working_dir);
    match image_prep::load_and_normalize(&resolved) {
        Ok(prep) => {
            debug!(
                path = %resolved.display(),
                "read_image: normalized image file"
            );
            Ok((resolved, prep))
        }
        Err(e) => {
            warn!(path = %resolved.display(), error = %e, "read_image: failed to read image");
            Err(ToolExecError(format!("failed to read image: {e}")))
        }
    }
}

impl super::Tool for ReadImage {
    type Args = ReadImageArgs;
    type Return = ReadImageReturn;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "read_image"
    }
    fn description(&self) -> &'static str {
        "Read an image file from the local workspace and provide it to a vision-capable model as image input. Returns the image's path, dimensions, and MIME type, and feeds the image to the model on the next request."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Reading image `{}`.", args.path)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.text.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&super::context::ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let (resolved, prep) = resolve_and_normalize(&args, working_dir)?;
        // Capture the byte length before moving `prep.data` into the reference
        // (the text handle and log below still need it).
        let byte_len = prep.data.len();
        let reference = ImageReference {
            path: resolved.display().to_string(),
            mime_type: prep.mime_type.to_string(),
            width: prep.width,
            height: prep.height,
            // Move the normalized bytes into the reference so the request
            // builder can attach them directly without re-reading the source.
            data: prep.data,
        };
        let text = truncate_tool_output(&format!(
            "read image: {} ({}x{}, {}, {})",
            resolved.display(),
            prep.width,
            prep.height,
            prep.mime_type,
            humfmt::bytes(byte_len as u64),
        ));
        info!(
            path = %resolved.display(),
            width = prep.width,
            height = prep.height,
            mime = %prep.mime_type,
            bytes = byte_len,
            "read_image: read image successfully"
        );
        // Carry the reference in the return value for the framework's
        // `extract_image_ref` hook to read from `ret` (no shared-state parking).
        Ok(ReadImageReturn { text, reference })
    }

    fn extract_image_ref(&self, ret: &Self::Return) -> Option<ImageReference> {
        Some(ret.reference.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tool as _;
    use super::*;
    use std::io::Write;

    fn run(path: &str) -> Result<String, ToolExecError> {
        let tool = ReadImage::new();
        tool.execute(
            ReadImageArgs {
                path: path.to_string(),
            },
            None,
            None,
            None,
        )
        .map(|ret| ret.text)
    }

    fn write_png() -> tempfile::NamedTempFile {
        // A tiny 3×2 opaque PNG via the image crate (used by image_prep tests).
        let buf = image::ImageBuffer::from_fn(3, 2, |x, y| {
            image::Rgb([(x * 80) as u8, (y * 90) as u8, 40])
        });
        let mut file = tempfile::NamedTempFile::new().unwrap();
        image::DynamicImage::ImageRgb8(buf)
            .write_to(&mut file, image::ImageFormat::Png)
            .unwrap();
        file
    }

    #[test]
    fn reads_image_and_reports_metadata() {
        let file = write_png();
        let out = run(&file.path().display().to_string()).unwrap();
        assert!(out.contains("read image:"), "{out}");
        assert!(out.contains("3x2"), "{out}");
        // Opaque PNG re-encodes to JPEG.
        assert!(out.contains("image/jpeg"), "{out}");
    }

    #[test]
    fn exposes_image_reference_via_hook() {
        let tool = ReadImage::new();
        let file = write_png();
        let args = ReadImageArgs {
            path: file.path().display().to_string(),
        };
        let ret = tool.execute(args, None, None, None).expect("execute");
        let reference = tool.extract_image_ref(&ret).expect("image ref");
        assert_eq!(reference.mime_type, "image/jpeg");
        assert_eq!(reference.width, 3);
        assert_eq!(reference.height, 2);
        assert_eq!(reference.path, file.path().display().to_string());
        // The reference is read off the return value, so it is present on every
        // call (no slot to drain).
        assert!(tool.extract_image_ref(&ret).is_some());
    }

    #[test]
    fn return_string_is_the_text_handle() {
        let tool = ReadImage::new();
        let file = write_png();
        let args = ReadImageArgs {
            path: file.path().display().to_string(),
        };
        let ret = tool.execute(args, None, None, None).expect("execute");
        assert_eq!(ReadImage::return_string(&ret), ret.text);
    }

    #[test]
    fn serializes_to_plain_string() {
        let ret = ReadImageReturn {
            text: "read image: hello".to_string(),
            reference: ImageReference {
                path: "/tmp/x.png".to_string(),
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                data: Vec::new(),
            },
        };
        let json = serde_json::to_string(&ret).unwrap();
        assert_eq!(json, r#""read image: hello""#);
    }

    #[test]
    fn svg_is_rasterized_and_reencoded_to_png() {
        // An SVG source is rasterized to RGBA (alpha → PNG), not rejected.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"<svg xmlns='http://www.w3.org/2000/svg' width='10' height='20'><rect width='10' height='20' fill='blue'/></svg>")
            .unwrap();
        let out = run(&file.path().display().to_string()).unwrap();
        assert!(out.contains("10x20"), "{out}");
        assert!(out.contains("image/png"), "{out}");
    }

    #[test]
    fn rejects_missing_path() {
        let err = run("  ").unwrap_err();
        assert!(err.to_string().contains("missing required"));
    }

    #[test]
    fn rejects_non_image_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"just text, not an image").unwrap();
        let err = run(&file.path().display().to_string()).unwrap_err();
        assert!(err.to_string().contains("failed to read image"), "{err}");
    }
}
