//! The `generate_image` tool: produce an image via the session's
//! image-capable provider and hand it to the client through the same
//! `PreparedImage` pipeline `display_image` uses.
//!
//! The tool is deliberately thin: provider resolution, model selection, and
//! the decode/prepare/return tail all live in the shared `image_common` spine
//! (`super::image_common`), which keeps provider resolution in the daemon
//! command loop (`DaemonCommand::GetImageGenerationProvider`, so the credential
//! never reaches a tool) and re-enters the exact prepare pipeline of
//! `display_image` (`prepare_image_from_bytes`) so the client's
//! display/persistence/vision-feedback path is shared and cannot drift.

use super::image::DisplayImageReturn;
use super::image_common::{finish_provider_image, pick_image_model, resolve_image_provider};
use super::{PreparedImage, ToolExecError, context::ToolContext};
use choreo_ai_protocols::images::{
    Background, ImageGenerationRequest, ImageQuality, ImageSize, OutputFormat,
};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

/// Arguments for `generate_image`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateImageArgs {
    /// What to depict. Photographic results come from photographic language
    /// (lens, lighting, film, camera detail); illustration phrasing produces
    /// illustration.
    prompt: String,
    /// Explicit image model id (e.g. "gpt-image-1", "imagen-3"). Optional —
    /// when omitted the provider's catalog image models are picked by
    /// priority. An invalid name yields the provider's clean 4xx.
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

/// The `generate_image` tool handle. Stateless: the provider, model, and
/// credentials are resolved per invocation through the daemon command loop.
pub struct GenerateImage {}

impl Default for GenerateImage {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerateImage {
    /// Construct the tool; equivalent to [`GenerateImage::default`].
    #[must_use]
    pub fn new() -> Self {
        GenerateImage {}
    }
}

impl super::Tool for GenerateImage {
    type Args = GenerateImageArgs;
    type Return = DisplayImageReturn;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "generate_image"
    }
    fn group(&self) -> &'static str {
        "image"
    }
    fn description(&self) -> &'static str {
        "Generate an image from a text prompt (photorealism from lens/lighting/camera-detail phrasing; illustration phrasing otherwise). Do NOT call for deadline, tracking, or status requests — only when the user actually wants an image produced."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Generating image from prompt: {}.", args.prompt)];
        if let Some(ref model) = args.model {
            parts.push(format!(" Model: `{model}`."));
        }
        if let Some(size) = args.size {
            // Display mirrors the wire strings ("1024x1024", "high", …), so
            // the invocation line matches what the API actually receives.
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
        _working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Cancellation is checked BEFORE anything (including the provider
        // round-trip), and again right after the round-trip returns (see the
        // post-generation check below). Once the HTTP request is in flight
        // it cannot be interrupted (the blocking client accepts no
        // mid-flight cancel) — v1 accepts that the in-flight generation
        // completes and its cost is sunk, but the result is discarded
        // instead of being decoded, persisted, and displayed.
        if let Some(ctx) = ctx
            && ctx.cancelled.load(Ordering::Relaxed)
        {
            warn!("generate_image: session cancelled before sending request");
            return Err(ToolExecError("image generation cancelled".to_string()));
        }

        let ctx = ctx.ok_or_else(|| {
            ToolExecError(
                "generate_image requires a session context (direct invocation without context is not supported)"
                    .to_string(),
            )
        })?;

        let handle = resolve_image_provider(ctx)?;

        // Model resolution: explicit arg > catalog-priority pick. The
        // catalog is the source of truth for what this provider can route at
        // all, and there is NO guessed fallback model: with no catalog
        // image-output candidates, the tool fails with guidance (overlay
        // entry or explicit `model` arg) rather than silently sending a
        // model the provider probably does not route (e.g. an opener-style
        // gateway would never serve `gpt-image-1`). Fail-safe over
        // fail-hopeful.
        let candidates = choreo_ai_protocols::image_models_for_provider(&handle.slug);
        let model = pick_image_model(&handle.slug, &candidates, args.model.as_deref())
            .map_err(ToolExecError)?;

        let size = args.size.unwrap_or_default();
        let quality = args.quality.unwrap_or_default();
        let output_format = args.output_format.unwrap_or_default();
        let background = args.background.unwrap_or_default();
        let request = ImageGenerationRequest {
            // `prompt` moves (args is owned and `alt` is a disjoint field,
            // so the partial move is fine) — no needless String clone.
            prompt: args.prompt,
            model: model.clone(),
            size,
            quality,
            output_format,
            background,
            // The tool does not expose a seed arg yet; deterministic
            // generation is opt-in and the knob is only honored by the fal
            // adapter. None here leaves every provider's body unchanged.
            seed: None,
        };

        // `None` cancel_rx: the blocking client accepts no mid-flight
        // cancel, and the pre-send flag check below is the authoritative
        // gate. Post-send abort IS still detected — the flag is re-checked
        // as soon as the round-trip returns, before any decode/display work
        // happens on the (possibly money-costing) result.
        let result = handle
            .client
            .generate_image(&request, None)
            .map_err(|e| ToolExecError(format!("image generation failed: {e}")))?;

        // Post-generation cancel check: the flag is re-tested before the
        // result is processed. A cancel issued while the (up to 180 s)
        // generation was in flight must not trigger a decode, validation,
        // persistence, or client display of the image — the generation cost
        // is sunk either way, but the downstream pipeline stays silent.
        if ctx.cancelled.load(Ordering::Relaxed) {
            warn!(
                "generate_image: session cancelled while generation was in flight — discarding result"
            );
            return Err(ToolExecError("image generation cancelled".to_string()));
        }

        // Decode, sniff the actual MIME, run the shared prepare pipeline, and
        // build the text handle — the tail shared with `edit_image`.
        let ret = finish_provider_image(&result, output_format, args.alt, "generated")?;

        // The operation-specific log line: the shared tail deliberately does
        // not log, so the provider slug and the final prepared size are
        // reported here (bytes are already in the return value).
        info!(
            model = %result.model,
            slug = %handle.slug,
            width = ret.image.width,
            height = ret.image.height,
            bytes = ret.image.data.len(),
            "generate_image: prepared generated image"
        );

        Ok(ret)
    }

    fn extract_image(&self, ret: &Self::Return) -> Option<PreparedImage> {
        Some(ret.image.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ImageProviderHandle;
    use crate::tools::Tool;
    use crate::tools::image::prepare_image_from_bytes;
    use crate::tools::image_common::sample_png_b64;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use choreo_ai_protocols::images::ImageGenerationClient;
    use choreo_ai_protocols::images::ImageGenerationResult;
    use choreo_proto::InferenceError;
    use std::sync::Arc;

    #[test]
    fn args_deserialize_and_require_prompt() {
        let args: GenerateImageArgs = serde_json::from_str(r#"{"prompt": "a cat"}"#).unwrap();
        assert_eq!(args.prompt, "a cat");
        assert!(args.model.is_none());
        assert!(args.size.is_none());
        // Missing prompt is a deserialization error (schema `required`).
        assert!(serde_json::from_str::<GenerateImageArgs>(r"{}").is_err());
        // Typed enum args deserialize from their wire strings.
        let args: GenerateImageArgs = serde_json::from_str(
            r#"{"prompt": "p", "size": "1024x1024", "quality": "high", "output_format": "webp", "background": "transparent"}"#,
        )
        .unwrap();
        assert_eq!(args.size, Some(ImageSize::Square1024));
        assert_eq!(args.quality, Some(ImageQuality::High));
        assert_eq!(args.output_format, Some(OutputFormat::Webp));
        assert_eq!(args.background, Some(Background::Transparent));
    }

    /// Stub client: Debug-able, returns a fixed image generation result.
    #[derive(Debug)]
    struct StubImageClient {
        png_b64: String,
        revised_prompt: Option<String>,
    }

    impl ImageGenerationClient for StubImageClient {
        // &'static str is the trait's required return lifetime for
        // `provider_slug`, not an over-bound string literal.
        #[expect(clippy::unnecessary_literal_bound)]
        fn provider_slug(&self) -> &str {
            "openai"
        }
        fn generate_image(
            &self,
            req: &ImageGenerationRequest,
            _cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
        ) -> Result<ImageGenerationResult, InferenceError> {
            // The stub ignores the cancel channel but must echo the request's
            // model; `revised_prompt` comes from the stub's field (the request
            // carries no prompt-rewrite). The `Some(..)` literal is the
            // per-test override path, hence the field.
            let revised_prompt = self.revised_prompt.clone();
            Ok(ImageGenerationResult {
                image_b64: self.png_b64.clone(),
                revised_prompt,
                model: req.model.clone(),
            })
        }
    }

    /// Run `GenerateImage::execute` against a mock daemon reply channel that
    /// returns the given handle.
    // Test helper whose panic paths are intentional test failures;
    // clippy::panic_in_result_fn has no allow-*-in-tests config option.
    #[expect(clippy::panic_in_result_fn)]
    fn execute_with_handle(
        handle: ImageProviderHandle,
        args: GenerateImageArgs,
        cancelled: bool,
    ) -> Result<DisplayImageReturn, ToolExecError> {
        // In-crate test DB: a throwaway redb file (same pattern as the
        // other tools' unit tests; `tempfile` owns the cleanup).
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let _dir_guard = dir;
        let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<crate::daemon::DaemonCommand>();
        // Mock daemon loop: reply to exactly one provider-resolution command
        // with the pre-built handle (never touching real credential state).
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
        GenerateImage::new().execute(args, None, None, Some(&ctx))
    }

    #[test]
    fn execute_happy_path_produces_display_image_return() {
        let png_b64 = sample_png_b64();
        let handle = ImageProviderHandle {
            slug: "openai".to_string(),
            client: Arc::new(StubImageClient {
                png_b64: png_b64.clone(),
                revised_prompt: Some("a rewritten prompt".to_string()),
            }),
        };
        let ret = execute_with_handle(
            handle,
            serde_json::from_str(r#"{"prompt": "a lighthouse", "model": "gpt-image-1"}"#).unwrap(),
            false,
        )
        .unwrap();

        // Dimensions probed from the real PNG; extract_image hands the image
        // to the client pipeline.
        assert_eq!((ret.image.width, ret.image.height), (4, 3));
        assert_eq!(ret.image.mime_type, "image/png");
        assert_eq!(
            GenerateImage::new().extract_image(&ret).unwrap().width,
            ret.image.width
        );
        // revised_prompt lands in the text handle along with
        // the model that produced the image.
        assert!(
            ret.text.contains("via gpt-image-1"),
            "text handle: {}",
            ret.text
        );
        assert!(
            ret.text.contains("revised prompt: a rewritten prompt"),
            "text handle: {}",
            ret.text
        );
    }

    #[test]
    fn execute_aborts_on_cancelled_flag() {
        let handle = ImageProviderHandle {
            slug: "openai".to_string(),
            client: Arc::new(StubImageClient {
                png_b64: sample_png_b64(),
                revised_prompt: None,
            }),
        };
        let err = execute_with_handle(
            handle,
            serde_json::from_str(r#"{"prompt": "p"}"#).unwrap(),
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn execute_without_context_errors_clearly() {
        let err = GenerateImage::new()
            .execute(
                serde_json::from_str(r#"{"prompt": "p"}"#).unwrap(),
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("requires a session context"),
            "{err}"
        );
    }

    #[test]
    fn prepare_path_rejects_oversized_and_unsupported_mime() {
        let png = BASE64.decode(sample_png_b64()).unwrap();
        // "image/gif" is supported; use a genuinely unsupported type.
        assert!(prepare_image_from_bytes("text/plain", &png).is_err());
        // Cap check: a "valid" payload over the limit is rejected before the
        // dimension probe.
        let big = vec![0u8; super::super::image::MAX_DISPLAY_IMAGE_BYTES + 1];
        assert!(prepare_image_from_bytes("image/png", &big).is_err());
    }
}
