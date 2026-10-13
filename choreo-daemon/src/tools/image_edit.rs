//! The `edit_image` tool: modify an existing image (named by local path) via the
//! session's image-capable provider, then hand the result to the client through
//! the same `PreparedImage` pipeline `display_image`/`generate_image` use.
//!
//! The tool is deliberately thin and mirrors `generate_image`: provider
//! resolution lives in the daemon command loop
//! (`DaemonCommand::GetImageGenerationProvider`, so the credential never reaches
//! a tool), model selection reuses the shared catalog-driven priority pick over
//! the provider's *edit-capable* candidates, and the returned bytes re-enter the
//! exact prepare pipeline of `display_image` (`prepare_image_from_bytes`), so
//! the client's display/persistence/vision-feedback path is shared and cannot
//! drift.
//!
//! Unlike `generate_image`, the inputs are either local file paths that are read
//! and normalized here (`image_prep::load_and_normalize`) before the provider
//! round trip, or **session references** (`@last`, `@display:<turn>:<index>`,
//! `@result:<turn>:<call_id>`) that name an image already stored in this session
//! — a prior `read_image` result or a `generate_image`/`edit_image`/`display_image`
//! output. A session reference is resolved through the daemon command loop
//! (`DaemonCommand::GetSessionImage`) and the returned bytes are used as-is
//! (they were normalized when the image was produced), so the adapter only ever
//! serializes provider-allowlisted bytes.

use super::image::{DisplayImageReturn, prepare_image_from_bytes};
use super::image_gen::{output_format_mime, pick_image_model};
use super::{PreparedImage, ToolExecError, context::ToolContext, truncate_tool_output};
use crate::daemon::SessionImageSelector;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::images::{
    Background, ImageEditRequest, ImageInput, ImageQuality, ImageSize, OutputFormat,
};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

/// Arguments for `edit_image`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditImageArgs {
    /// What to change about the input image(s). Describe the edit, not the
    /// whole image ("add a red hat", "remove the background").
    prompt: String,
    /// One or more input images to edit (at least one). Each entry is either a
    /// local **file path** or a **session reference** to an image already in
    /// this session — `@last` (the most recent image), `@display:<turn>:<index>`,
    /// or `@result:<turn>:<call_id>`. Use `@last` to edit the image the user
    /// just shared or that a previous `generate_image`/`edit_image` produced.
    images: Vec<String>,
    /// Optional path to a PNG inpainting mask. The mask's **transparent**
    /// region is the area to edit/regenerate; the opaque region is kept
    /// unchanged. Send only on providers/models that support inpainting.
    mask: Option<String>,
    /// Explicit image-editing model id. Optional — when omitted the provider's
    /// catalog edit-capable models are picked by priority. An invalid name
    /// yields the provider's clean 4xx.
    model: Option<String>,
    /// Output canvas size ("auto", "1024x1024", "1024x1536", "1536x1024").
    size: Option<ImageSize>,
    /// Render quality hint ("auto", "low", "medium", "high").
    quality: Option<ImageQuality>,
    /// Encoded format of the returned image ("png", "jpeg", "webp").
    output_format: Option<OutputFormat>,
    /// Background handling ("auto", "opaque", "transparent" — transparent
    /// requires png/webp).
    background: Option<Background>,
    /// Alt text for the displayed image.
    alt: Option<String>,
}

/// The `edit_image` tool handle. Stateless: the provider, model, and credentials
/// are resolved per invocation through the daemon command loop.
pub struct EditImage {}

impl Default for EditImage {
    fn default() -> Self {
        Self::new()
    }
}

impl EditImage {
    /// Construct the tool; equivalent to [`EditImage::default`].
    #[must_use]
    pub fn new() -> Self {
        EditImage {}
    }
}

/// The error for an `images` entry that starts with `@` but is not a recognized
/// session reference. Names the bad entry and the accepted grammar so the model
/// can correct the call.
fn invalid_selector(entry: &str) -> ToolExecError {
    ToolExecError(format!(
        "edit_image: unrecognized session image reference `{entry}` — use `@last`, \
         `@display:<turn>:<index>`, or `@result:<turn>:<call_id>`, or pass a file path"
    ))
}

/// Parse one `edit_image` `images` entry into a [`SessionImageSelector`] when it
/// is a session reference (a leading `@`), or `None` when it is a filesystem
/// path.
///
/// Grammar (the leading `@` is the discriminator; anything else is a path):
///
/// - `@last` — the most recent image attachment in this session.
/// - `@display:<turn>:<index>` — a displayed image, by turn id and position.
/// - `@result:<turn>:<call_id>` — a tool-result vision image, by turn id and
///   the call id that produced it.
///
/// An unrecognized `@…` form is an error, not a filename: silently treating
/// `@lst` (a typo for `@last`) as a path would send a confusing "file not
/// found" instead of naming the bad selector.
fn parse_session_selector(entry: &str) -> Result<Option<SessionImageSelector>, ToolExecError> {
    let Some(rest) = entry.strip_prefix('@') else {
        return Ok(None);
    };
    let selector = if rest == "last" {
        SessionImageSelector::Last
    } else if let Some(args) = rest.strip_prefix("display:") {
        let (turn, index) = parse_two_u32(args).ok_or_else(|| invalid_selector(entry))?;
        SessionImageSelector::Display { turn, index }
    } else if let Some(args) = rest.strip_prefix("result:") {
        let (turn, call_id) = args
            .split_once(':')
            .ok_or_else(|| invalid_selector(entry))?;
        let turn = turn.parse::<u32>().map_err(|_| invalid_selector(entry))?;
        if call_id.is_empty() {
            return Err(invalid_selector(entry));
        }
        SessionImageSelector::Result {
            turn,
            call_id: call_id.to_string(),
        }
    } else {
        return Err(invalid_selector(entry));
    };
    Ok(Some(selector))
}

/// Parse `<a>:<b>` where both parts are `u32`, or `None` when either is absent
/// or malformed. Shared by the `@display:<turn>:<index>` form.
fn parse_two_u32(args: &str) -> Option<(u32, u32)> {
    let (a, b) = args.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

impl super::Tool for EditImage {
    type Args = EditImageArgs;
    type Return = DisplayImageReturn;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "edit_image"
    }
    fn group(&self) -> &'static str {
        "image"
    }
    fn description(&self) -> &'static str {
        "Edit an existing image (image-to-image): modify the input image(s) according to a text prompt. Each input is either a local file path or a session reference to an image already in this session (`@last`, `@display:<turn>:<index>`, `@result:<turn>:<call_id>`) — use `@last` to edit the image the user just shared or that a previous generate_image/edit_image produced. Do NOT use to create an image from scratch (use generate_image) or to merely display one (use display_image)."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!(
            "Editing {} image(s) from prompt: {}.",
            args.images.len(),
            args.prompt
        )];
        for path in &args.images {
            parts.push(format!(" Input: `{path}`."));
        }
        if let Some(ref mask) = args.mask {
            parts.push(format!(" Mask: `{mask}`."));
        }
        if let Some(ref model) = args.model {
            parts.push(format!(" Model: `{model}`."));
        }
        if let Some(size) = args.size {
            parts.push(format!(" Size: {size}."));
        }
        if let Some(quality) = args.quality {
            parts.push(format!(" Quality: {quality}."));
        }
        if let Some(format) = args.output_format {
            parts.push(format!(" Format: {format}."));
        }
        if let Some(background) = args.background {
            parts.push(format!(" Background: {background}."));
        }
        if let Some(ref alt) = args.alt {
            parts.push(format!(" Alt text: {alt}."));
        }
        parts.concat()
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.text.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Cancellation is checked BEFORE any work (path reads included), and
        // again right after the provider round-trip returns (see the
        // post-response check below). The blocking client accepts no mid-flight
        // cancel, so an in-flight edit completes and its cost is sunk — but the
        // result is discarded rather than decoded, persisted, and displayed.
        if let Some(ctx) = ctx
            && ctx.cancelled.load(Ordering::Relaxed)
        {
            warn!("edit_image: session cancelled before sending request");
            return Err(ToolExecError("image edit cancelled".to_string()));
        }

        if args.images.is_empty() {
            return Err(ToolExecError(
                "edit_image requires at least one input image".to_string(),
            ));
        }

        let ctx = ctx.ok_or_else(|| {
            ToolExecError(
                "edit_image requires a session context (direct invocation without context is not supported)"
                    .to_string(),
            )
        })?;

        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.daemon_tx
            .send(crate::daemon::DaemonCommand::GetImageGenerationProvider {
                // The requesting session scopes the image sockets: the daemon
                // builds the client against THAT session's registry, so a
                // cancel closes an in-flight image request too.
                session_id: ctx.session_id,
                account_name: ctx.account_name.clone(),
                reply: tx,
            })
            .map_err(|e| {
                ToolExecError(format!(
                    "failed to reach the daemon command loop for image provider resolution: {e}"
                ))
            })?;
        let handle = rx.recv().map_err(|e| {
            ToolExecError(format!(
                "daemon dropped the image provider reply channel: {e}"
            ))
        })?;
        // Map the structured daemon error into its Display text so the model
        // still sees the precise guidance (unlock / account / slug wording).
        let handle = handle.map_err(|e| ToolExecError(e.to_string()))?;

        // Model resolution: explicit arg > catalog-priority pick. The
        // candidates come from the provider's *edit-capable* models, and there
        // is NO guessed fallback — with no candidates (and no explicit model)
        // the tool fails closed with guidance rather than sending a
        // generation-only model to an edit endpoint it cannot serve.
        let candidates = choreo_ai_protocols::image_edit_models_for_provider(&handle.slug);
        if candidates.is_empty() && args.model.is_none() {
            return Err(ToolExecError(format!(
                "provider `{}` has no edit-capable models in the catalog — pass `model` \
                 explicitly if your endpoint routes one, or add a `supports_image_edit` \
                 overlay entry for it and /refresh-models",
                handle.slug
            )));
        }
        let model = pick_image_model(&handle.slug, &candidates, args.model.as_deref())
            .map_err(ToolExecError)?;

        // Build every input up front. A `@…` entry is a session reference
        // resolved through the daemon command loop (the daemon reads the
        // session's stored image and replies with its MIME + already-normalized
        // bytes, used as-is — no re-normalization); any other entry is a local
        // path, read and normalized here. `load_and_normalize` re-encodes to a
        // provider-allowlisted format (png/jpeg) and applies the
        // decompression-bomb guard, so the adapter serializes only bounded,
        // known-good bytes in either case.
        let mut inputs = Vec::with_capacity(args.images.len());
        for entry in &args.images {
            if let Some(selector) = parse_session_selector(entry)? {
                // The display filename the provider records is the selector
                // text itself (e.g. "@last"), a sane suffix hint for the
                // stored bytes' MIME.
                let filename = selector.to_string();
                let (tx, rx) = crossbeam_channel::unbounded();
                ctx.daemon_tx
                    .send(crate::daemon::DaemonCommand::GetSessionImage {
                        session_id: ctx.session_id,
                        selector,
                        reply: tx,
                    })
                    .map_err(|e| {
                        ToolExecError(format!(
                            "failed to reach the daemon command loop for image `{entry}`: {e}"
                        ))
                    })?;
                let resolved = rx.recv().map_err(|e| {
                    ToolExecError(format!(
                        "daemon dropped the session-image reply channel for `{entry}`: {e}"
                    ))
                })?;
                // Map the structured daemon error into its Display text so the
                // model sees the precise guidance (no-image / bad-reference
                // wording).
                let resolved = resolved.map_err(|e| ToolExecError(format!("edit_image: {e}")))?;
                inputs.push(ImageInput {
                    filename,
                    mime_type: resolved.mime_type,
                    data: resolved.data,
                });
            } else {
                let resolved = super::resolve_path(entry, working_dir);
                let prepared =
                    crate::image_prep::load_and_normalize(&resolved, None).map_err(|e| {
                        ToolExecError(format!(
                            "edit_image: failed to load input image `{entry}`: {e}"
                        ))
                    })?;
                let filename = resolved
                    .file_name()
                    .map_or_else(|| "image".to_string(), |n| n.to_string_lossy().into_owned());
                inputs.push(ImageInput {
                    filename,
                    mime_type: prepared.mime_type.to_string(),
                    data: prepared.data,
                });
            }
        }
        let input_count = inputs.len();
        let total_input_bytes: usize = inputs.iter().map(|i| i.data.len()).sum();

        // Resolve the optional inpainting mask. Unlike the input images it is
        // normalized with `load_and_normalize_mask`, which forces a PNG
        // re-encode: the mask's transparent region is the edit target and JPEG
        // has no alpha channel, so a JPEG'd mask would lose the very region
        // the "transparent = editable" convention depends on.
        let mask = match &args.mask {
            Some(path) => {
                let resolved = super::resolve_path(path, working_dir);
                let prepared =
                    crate::image_prep::load_and_normalize_mask(&resolved).map_err(|e| {
                        ToolExecError(format!("edit_image: failed to load mask `{path}`: {e}"))
                    })?;
                let filename = resolved.file_name().map_or_else(
                    || "mask.png".to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                Some(ImageInput {
                    filename,
                    mime_type: prepared.mime_type.to_string(),
                    data: prepared.data,
                })
            }
            None => None,
        };
        let has_mask = mask.is_some();
        let mask_bytes = mask.as_ref().map_or(0, |m| m.data.len());

        let size = args.size.unwrap_or_default();
        let quality = args.quality.unwrap_or_default();
        let output_format = args.output_format.unwrap_or_default();
        let background = args.background.unwrap_or_default();
        let request = ImageEditRequest {
            // `prompt` moves (args is owned and the other fields are disjoint).
            prompt: args.prompt,
            model,
            images: inputs,
            mask,
            size,
            quality,
            output_format,
            background,
        };

        // `None` cancel_rx: the blocking client accepts no mid-flight cancel,
        // and the flag checks before/after the round-trip are the gates.
        let result = handle
            .client
            .edit_image(&request, None)
            .map_err(|e| ToolExecError(format!("image edit failed: {e}")))?;

        // Post-response cancel check: a cancel issued while the (up to 180 s)
        // edit was in flight must not trigger a decode, validation,
        // persistence, or client display of the image — the cost is sunk
        // either way, but the downstream pipeline stays silent.
        if ctx.cancelled.load(Ordering::Relaxed) {
            warn!("edit_image: session cancelled while the edit was in flight — discarding result");
            return Err(ToolExecError("image edit cancelled".to_string()));
        }

        let bytes = BASE64.decode(result.image_b64.trim()).map_err(|e| {
            ToolExecError(format!(
                "provider returned malformed base64 image data: {e}"
            ))
        })?;
        let byte_len = bytes.len();
        let mime_type = output_format_mime(output_format);
        let (mime_type, width, height) = prepare_image_from_bytes(mime_type, &bytes)
            .map_err(|e| ToolExecError(format!("edited image failed validation: {e}")))?;

        info!(
            model = %result.model,
            slug = %handle.slug,
            input_count,
            total_input_bytes,
            has_mask,
            mask_bytes,
            width,
            height,
            bytes = byte_len,
            "edit_image: prepared edited image"
        );

        // Text handle mirrors `generate_image`'s, plus the provider's revised
        // prompt when it returns one.
        let revised = match result.revised_prompt.as_deref() {
            Some(prompt) => format!("\nrevised prompt: {prompt}"),
            None => String::new(),
        };
        let text = truncate_tool_output(&format!(
            "edited image ({mime_type}, {width}x{height}, {}) via {result_model}{revised}",
            humfmt::bytes(byte_len as u64),
            result_model = result.model,
        ));

        Ok(DisplayImageReturn {
            text,
            image: PreparedImage {
                mime_type,
                data: bytes,
                width,
                height,
                alt: args.alt.filter(|alt| !alt.trim().is_empty()),
            },
        })
    }

    fn extract_image(&self, ret: &Self::Return) -> Option<PreparedImage> {
        Some(ret.image.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{ResolvedImage, SessionImageError};
    use crate::providers::ImageProviderHandle;
    use crate::tools::Tool;
    use choreo_ai_protocols::images::{
        ImageEditRequest, ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult,
    };
    use choreo_proto::InferenceError;
    use image::ImageFormat;
    use std::io::Cursor;
    use std::sync::Arc;

    /// Build a real 4x3 PNG, return its base64 (mirrors the image.rs tests).
    fn sample_png_b64() -> String {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(4, 3, |x, y| {
            #[expect(clippy::cast_possible_truncation)]
            image::Rgba([x as u8 * 60, y as u8 * 80, 0, 255])
        }));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, ImageFormat::Png).unwrap();
        BASE64.encode(png.into_inner())
    }

    /// Stub client: generation is unsupported; editing returns a fixed image.
    #[derive(Debug)]
    struct StubEditClient {
        png_b64: String,
        revised_prompt: Option<String>,
        /// Records the input count seen by the last edit call, so a test can
        /// assert the tool forwarded every path.
        last_input_count: std::sync::atomic::AtomicUsize,
        /// Whether the last edit call carried a mask at all.
        last_had_mask: std::sync::atomic::AtomicBool,
        /// Whether that mask was normalized to `image/png` (it always must be).
        last_mask_is_png: std::sync::atomic::AtomicBool,
        /// The bytes of the first input image forwarded to the last edit call,
        /// so a test can prove a session-referenced image is passed through
        /// verbatim (not re-normalized).
        last_first_input: std::sync::Mutex<Vec<u8>>,
    }

    impl ImageGenerationClient for StubEditClient {
        #[expect(clippy::unnecessary_literal_bound)]
        fn provider_slug(&self) -> &str {
            "openai"
        }
        fn generate_image(
            &self,
            _req: &ImageGenerationRequest,
            _cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
        ) -> Result<ImageGenerationResult, InferenceError> {
            Err(InferenceError::Unsupported {
                detail: "stub has no generation".to_string(),
            })
        }
        fn edit_image(
            &self,
            req: &ImageEditRequest,
            _cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
        ) -> Result<ImageGenerationResult, InferenceError> {
            self.last_input_count
                .store(req.images.len(), std::sync::atomic::Ordering::SeqCst);
            self.last_had_mask
                .store(req.mask.is_some(), std::sync::atomic::Ordering::SeqCst);
            self.last_mask_is_png.store(
                req.mask
                    .as_ref()
                    .is_some_and(|m| m.mime_type == "image/png"),
                std::sync::atomic::Ordering::SeqCst,
            );
            *self.last_first_input.lock().unwrap() =
                req.images.first().map_or_default(|i| i.data.clone());
            Ok(ImageGenerationResult {
                image_b64: self.png_b64.clone(),
                revised_prompt: self.revised_prompt.clone(),
                model: req.model.clone(),
            })
        }
    }

    /// Run `EditImage::execute` against a mock daemon reply channel returning
    /// the given handle, inside a temp working dir holding `input.png`.
    #[expect(clippy::panic_in_result_fn)]
    fn execute_with_handle(
        handle: ImageProviderHandle,
        args: EditImageArgs,
        cancelled: bool,
    ) -> Result<DisplayImageReturn, ToolExecError> {
        let dir = tempfile::tempdir().expect("temp dir");
        // Write a real PNG the tool can normalize + read (RGBA, so the mask
        // path keeps it as PNG).
        std::fs::write(
            dir.path().join("input.png"),
            BASE64.decode(sample_png_b64()).expect("valid png"),
        )
        .expect("write input");
        std::fs::write(
            dir.path().join("mask.png"),
            BASE64.decode(sample_png_b64()).expect("valid png"),
        )
        .expect("write mask");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let working_dir = dir.path().to_path_buf();
        let _dir_guard = dir;
        let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<crate::daemon::DaemonCommand>();
        std::thread::spawn(move || match daemon_rx.recv() {
            Ok(crate::daemon::DaemonCommand::GetImageGenerationProvider { reply, .. }) => {
                let _ = reply.send(Ok(handle));
            }
            Ok(_) => panic!("mock daemon received unexpected command"),
            Err(_) => {}
        });
        let mut ctx = ToolContext::new(1, db, daemon_tx);
        if cancelled {
            use std::sync::atomic::AtomicBool;
            ctx.cancelled = Arc::new(AtomicBool::new(true));
        }
        EditImage::new().execute(args, None, Some(&working_dir), Some(&ctx))
    }

    fn openai_handle(revised_prompt: Option<String>) -> (ImageProviderHandle, Arc<StubEditClient>) {
        let client = Arc::new(StubEditClient {
            png_b64: sample_png_b64(),
            revised_prompt,
            last_input_count: std::sync::atomic::AtomicUsize::new(0),
            last_had_mask: std::sync::atomic::AtomicBool::new(false),
            last_mask_is_png: std::sync::atomic::AtomicBool::new(false),
            last_first_input: std::sync::Mutex::new(Vec::new()),
        });
        let handle = ImageProviderHandle {
            slug: "openai".to_string(),
            client: client.clone(),
        };
        (handle, client)
    }

    /// Run `EditImage::execute` against a mock daemon that answers BOTH the
    /// provider resolution and every `GetSessionImage` — the latter with a fixed
    /// resolved image (or error), so a session-reference test never touches real
    /// session state.
    #[expect(clippy::panic_in_result_fn)]
    fn execute_with_session_image(
        handle: ImageProviderHandle,
        resolved: Result<ResolvedImage, SessionImageError>,
        args: EditImageArgs,
    ) -> Result<DisplayImageReturn, ToolExecError> {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let _dir_guard = dir;
        let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<crate::daemon::DaemonCommand>();
        std::thread::spawn(move || {
            while let Ok(cmd) = daemon_rx.recv() {
                match cmd {
                    crate::daemon::DaemonCommand::GetImageGenerationProvider { reply, .. } => {
                        let _ = reply.send(Ok(handle.clone()));
                    }
                    crate::daemon::DaemonCommand::GetSessionImage { reply, .. } => {
                        let _ = reply.send(resolved.clone());
                    }
                    _ => panic!("mock daemon received unexpected command"),
                }
            }
        });
        let ctx = ToolContext::new(1, db, daemon_tx);
        EditImage::new().execute(args, None, None, Some(&ctx))
    }

    #[test]
    fn args_deserialize_and_require_prompt() {
        let args: EditImageArgs =
            serde_json::from_str(r#"{"prompt": "add a hat", "images": ["a.png"]}"#).unwrap();
        assert_eq!(args.prompt, "add a hat");
        assert_eq!(args.images, vec!["a.png".to_string()]);
        assert!(args.model.is_none());
        // Missing prompt is a deserialization error.
        assert!(serde_json::from_str::<EditImageArgs>(r#"{"images": ["a.png"]}"#).is_err());
        // Typed enum args deserialize from their wire strings.
        let args: EditImageArgs = serde_json::from_str(
            r#"{"prompt": "p", "images": ["a.png"], "size": "1024x1024", "quality": "high", "output_format": "webp", "background": "transparent"}"#,
        )
        .unwrap();
        assert_eq!(args.size, Some(ImageSize::Square1024));
        assert_eq!(args.quality, Some(ImageQuality::High));
        assert_eq!(args.output_format, Some(OutputFormat::Webp));
        assert_eq!(args.background, Some(Background::Transparent));
        // An optional mask path deserializes into the `mask` field.
        assert!(args.mask.is_none());
        let args: EditImageArgs =
            serde_json::from_str(r#"{"prompt": "p", "images": ["a.png"], "mask": "m.png"}"#)
                .unwrap();
        assert_eq!(args.mask.as_deref(), Some("m.png"));
    }

    #[test]
    fn execute_rejects_empty_images() {
        // No context is needed to reject the empty list; the check runs before
        // provider resolution.
        let err = EditImage::new()
            .execute(
                serde_json::from_str(r#"{"prompt": "p", "images": []}"#).unwrap(),
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("at least one input image"),
            "{err}"
        );
    }

    #[test]
    fn execute_happy_path_produces_display_image_return() {
        let (handle, client) = openai_handle(Some("a rewritten prompt".to_string()));
        let ret = execute_with_handle(
            handle,
            serde_json::from_str(
                r#"{"prompt": "add a hat", "images": ["input.png"], "model": "gpt-image-1"}"#,
            )
            .unwrap(),
            false,
        )
        .unwrap();

        // The input path was forwarded to the adapter.
        assert_eq!(
            client
                .last_input_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        // Dimensions probed from the real PNG; extract_image hands the image to
        // the client pipeline.
        assert_eq!((ret.image.width, ret.image.height), (4, 3));
        assert_eq!(ret.image.mime_type, "image/png");
        assert_eq!(
            EditImage::new().extract_image(&ret).unwrap().width,
            ret.image.width
        );
        assert!(ret.text.contains("via gpt-image-1"), "text: {}", ret.text);
        assert!(
            ret.text.contains("revised prompt: a rewritten prompt"),
            "text: {}",
            ret.text
        );
    }

    #[test]
    fn execute_aborts_on_cancelled_flag() {
        let (handle, _client) = openai_handle(None);
        let err = execute_with_handle(
            handle,
            serde_json::from_str(r#"{"prompt": "p", "images": ["input.png"]}"#).unwrap(),
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn execute_normalizes_mask_and_forwards_it_as_png() {
        let (handle, client) = openai_handle(None);
        execute_with_handle(
            handle,
            serde_json::from_str(
                r#"{"prompt": "inpaint", "images": ["input.png"], "mask": "mask.png", "model": "gpt-image-1"}"#,
            )
            .unwrap(),
            false,
        )
        .unwrap();
        // The mask path was loaded, reached the adapter, and stayed PNG.
        assert!(
            client
                .last_had_mask
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(
            client
                .last_mask_is_png
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn execute_rejects_a_bad_mask_path() {
        let (handle, _client) = openai_handle(None);
        let err = execute_with_handle(
            handle,
            serde_json::from_str(
                r#"{"prompt": "p", "images": ["input.png"], "mask": "missing.png", "model": "gpt-image-1"}"#,
            )
            .unwrap(),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to load mask"), "{err}");
    }

    #[test]
    fn execute_fails_closed_with_no_edit_candidates() {
        // No explicit `model` and an empty edit-candidate list must fail with
        // clean guidance rather than sending a generation-only model. The
        // bundled catalog has no edit models under a bogus slug.
        let (handle, _client) = openai_handle(None);
        let handle = ImageProviderHandle {
            slug: "definitely-not-a-provider".to_string(),
            client: handle.client,
        };
        let err = execute_with_handle(
            handle,
            serde_json::from_str(r#"{"prompt": "p", "images": ["input.png"]}"#).unwrap(),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no edit-capable models"), "{err}");
    }

    #[test]
    fn parse_session_selector_distinguishes_refs_from_paths() {
        assert_eq!(
            parse_session_selector("@last").unwrap(),
            Some(SessionImageSelector::Last)
        );
        // Non-`@` entries are paths, not selectors.
        assert_eq!(parse_session_selector("input.png").unwrap(), None);
        assert_eq!(parse_session_selector("/tmp/a b.png").unwrap(), None);
        // Explicit references.
        assert_eq!(
            parse_session_selector("@display:2:1").unwrap(),
            Some(SessionImageSelector::Display { turn: 2, index: 1 })
        );
        assert_eq!(
            parse_session_selector("@result:3:call_abc").unwrap(),
            Some(SessionImageSelector::Result {
                turn: 3,
                call_id: "call_abc".to_string()
            })
        );
        // A malformed `@…` form is an error, not silently a path.
        assert!(parse_session_selector("@lst").is_err());
        assert!(parse_session_selector("@display:2").is_err());
        assert!(parse_session_selector("@display:x:0").is_err());
        assert!(parse_session_selector("@result:3").is_err());
        assert!(parse_session_selector("@result:3:").is_err());
    }

    #[test]
    fn execute_passes_a_session_image_through_unmodified() {
        let (handle, client) = openai_handle(None);
        // Deliberately NOT a valid image: if the tool re-normalized the bytes it
        // would reject them — passing them straight to the adapter proves the
        // daemon-returned (already-normalized) bytes are forwarded verbatim.
        let raw = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let ret = execute_with_session_image(
            handle,
            Ok(ResolvedImage {
                mime_type: "image/png".to_string(),
                data: raw.clone(),
            }),
            serde_json::from_str(
                r#"{"prompt": "make it blue", "images": ["@last"], "model": "gpt-image-1"}"#,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            client
                .last_input_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(*client.last_first_input.lock().unwrap(), raw);
        assert_eq!((ret.image.width, ret.image.height), (4, 3));
    }

    #[test]
    fn execute_surfaces_a_no_images_selector_error() {
        let (handle, _client) = openai_handle(None);
        let err = execute_with_session_image(
            handle,
            Err(SessionImageError::NoImages {
                selector: "@last".to_string(),
            }),
            serde_json::from_str(r#"{"prompt": "p", "images": ["@last"], "model": "gpt-image-1"}"#)
                .unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no images"), "{err}");
    }

    #[test]
    fn execute_rejects_a_malformed_selector() {
        let (handle, _client) = openai_handle(None);
        let err = execute_with_session_image(
            handle,
            Ok(ResolvedImage {
                mime_type: "image/png".to_string(),
                data: vec![1, 2, 3],
            }),
            serde_json::from_str(r#"{"prompt": "p", "images": ["@lst"], "model": "gpt-image-1"}"#)
                .unwrap(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unrecognized session image reference"),
            "{err}"
        );
    }
}
