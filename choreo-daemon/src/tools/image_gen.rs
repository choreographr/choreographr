//! The `generate_image` tool: produce an image via the session's
//! image-capable provider and hand it to the client through the same
//! `PreparedImage` pipeline `display_image` uses.
//!
//! The tool is deliberately thin: provider resolution lives in the daemon
//! command loop (`DaemonCommand::GetImageGenerationProvider`, so the
//! credential never reaches a tool), model selection is a small pure
//! catalog-driven function below, and the returned bytes re-enter the exact
//! prepare pipeline of `display_image` (`prepare_image_from_bytes`) so the
//! client's display/persistence/vision-feedback path is shared and cannot
//! drift.

use super::image::{DisplayImageReturn, prepare_image_from_bytes};
use super::{PreparedImage, ToolExecError, context::ToolContext, truncate_tool_output};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::images::{
    Background, ImageGenerationRequest, ImageQuality, ImageSize, OutputFormat,
};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

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

/// Map the requested output format to the MIME type the returned bytes are
/// decoded with. The adapter requests the format explicitly and the API
/// returns exactly it, so this mapping is exhaustive — there is nothing to
/// sniff.
fn output_format_mime(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png => "image/png",
        OutputFormat::Jpeg => "image/jpeg",
        OutputFormat::Webp => "image/webp",
    }
}

/// Pick the image model for a provider.
///
/// `explicit` (the tool's `model` arg) always wins — the model typed the name
/// and the API reports bad names as a clean 4xx, which is the right failure
/// for a deliberate pass-through. Otherwise the choice is made ONLY among
/// catalog-verified candidates (`image_models_for_provider`), never guessed:
/// a model that is not in the provider's catalog entry is unlikely to be
/// routable through the account. The priority ordering (ironclaw-style):
/// gpt-image > imagen > gemini-image > flux > dall-e — "best first" by
/// fidelity/cost behavior, with the legacy dall-e family explicitly last.
fn pick_image_model(
    slug: &str,
    candidates: &[String],
    explicit: Option<&str>,
) -> Result<String, String> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_string());
    }
    if candidates.is_empty() {
        return Err(format!(
            "provider `{slug}` has no image-output models in the catalog — pass `model` explicitly if your endpoint proxies one"
        ));
        // NOTE: `execute` does not surface this error verbatim — it degrades
        // to the client's `default_image_model()` (see the resolution chain
        // there) so a lagging catalog snapshot cannot hard-fail the tool.
    }

    // Priority tiers by case-insensitive substring match, first match wins.
    let tiers: [(&str, Option<&str>); 5] = [
        ("gpt-image", None),
        ("imagen", None),
        ("gemini-", Some("image")),
        ("flux", None),
        ("dall-e", None),
    ];
    // Candidate names are lowercased ONCE up front instead of per tier per
    // candidate — the tier loop is ×5, and the catalog list can be long.
    let lowered: Vec<(String, String)> = candidates
        .iter()
        .map(|c| (c.to_ascii_lowercase(), c.clone()))
        .collect();
    for (primary, secondary) in tiers {
        let needle_p = primary.to_ascii_lowercase();
        for (hay, original) in &lowered {
            if !hay.contains(&needle_p) {
                continue;
            }
            // "gemini-" + "image": both substrings must be present (a bare
            // gemini chat model would otherwise match the tier).
            if secondary.is_some_and(|sec| !hay.contains(sec)) {
                continue;
            }
            return Ok(original.clone());
        }
    }
    // No tier matched: the catalog's first listed image-capable model is the
    // least-wrong default (catalog order is quality-curated upstream).
    Ok(candidates[0].clone())
}

pub struct GenerateImage {}

impl Default for GenerateImage {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerateImage {
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

        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.daemon_tx
            .send(crate::daemon::DaemonCommand::GetImageGenerationProvider {
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
        let handle = handle.map_err(ToolExecError)?;

        // Model resolution: explicit arg > catalog-priority pick > the
        // client's own default model. The catalog is the source of truth for
        // what this provider can route at all; when it lists no image models
        // (a lagging snapshot, or a proxy account with no overlay entry) the
        // adapter's default (e.g. `gpt-image-1`) is the authoritative choice
        // for the wire family it speaks, so we degrade to that instead of
        // hard-failing the whole tool.
        let candidates = choreo_ai_protocols::image_models_for_provider(&handle.slug);
        let model = match pick_image_model(&handle.slug, &candidates, args.model.as_deref()) {
            Ok(model) => model,
            Err(miss) => {
                let fallback = handle.client.default_image_model().to_string();
                warn!(
                    slug = %handle.slug,
                    miss = %miss,
                    fallback = %fallback,
                    "generate_image: no catalog image models for provider — using the client's default image model"
                );
                fallback
            }
        };

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

        let bytes = BASE64.decode(result.image_b64.trim()).map_err(|e| {
            ToolExecError(format!(
                "provider returned malformed base64 image data: {e}"
            ))
        })?;
        let byte_len = bytes.len();
        let mime_type = output_format_mime(output_format);
        let (mime_type, width, height) = prepare_image_from_bytes(mime_type, &bytes)
            .map_err(|e| ToolExecError(format!("generated image failed validation: {e}")))?;

        info!(
            model = %result.model,
            slug = %handle.slug,
            width,
            height,
            bytes = byte_len,
            "generate_image: prepared generated image"
        );

        // Text handle mirrors display_image's, plus the model and the
        // provider's revised prompt — the model benefits from seeing how the
        // provider rewrote its request (codex pattern).
        let revised = result
            .revised_prompt
            .as_deref()
            .map(|p| format!("\nrevised prompt: {p}"))
            .unwrap_or_default();
        let text = truncate_tool_output(&format!(
            "generated image ({mime_type}, {width}x{height}, {}) via {result_model}{revised}",
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
    use crate::providers::ImageProviderHandle;
    use crate::tools::Tool;
    use choreo_ai_protocols::images::ImageGenerationClient;
    use choreo_ai_protocols::images::ImageGenerationResult;
    use choreo_proto::InferenceError;
    use image::ImageFormat;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::mpsc;

    fn candidates(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_image_model_explicit_wins() {
        let list = candidates(&["gpt-image-1", "flux-1"]);
        assert_eq!(
            pick_image_model("openai", &list, Some("flux-1")).unwrap(),
            "flux-1"
        );
        // Explicit wins even when the name is not in the catalog: the API
        // gives a clean 4xx for bad names, which is the pass-through contract.
        assert_eq!(
            pick_image_model("openai", &list, Some("my-proxy-model")).unwrap(),
            "my-proxy-model"
        );
    }

    #[test]
    fn pick_image_model_gpt_image_beats_flux() {
        let list = candidates(&["flux-1.1-pro", "gpt-image-1"]);
        assert_eq!(pick_image_model("x", &list, None).unwrap(), "gpt-image-1");
    }

    #[test]
    fn pick_image_model_imagen_before_gemini_image() {
        let list = candidates(&["gemini-2.0-flash-preview-image-generation", "imagen-3"]);
        assert_eq!(pick_image_model("x", &list, None).unwrap(), "imagen-3");
    }

    #[test]
    fn pick_image_model_gemini_image_requires_both_substrings() {
        // A bare gemini chat model does not satisfy the gemini-image tier;
        // the composite one does.
        let list = candidates(&[
            "gemini-2.0-flash",
            "gemini-2.0-flash-preview-image-generation",
        ]);
        assert_eq!(
            pick_image_model("x", &list, None).unwrap(),
            "gemini-2.0-flash-preview-image-generation"
        );
    }

    #[test]
    fn pick_image_model_dall_e_last_and_fallback_first() {
        let list = candidates(&["dall-e-3", "flux-schnell"]);
        assert_eq!(pick_image_model("x", &list, None).unwrap(), "flux-schnell");
        let only = candidates(&["dall-e-3"]);
        assert_eq!(pick_image_model("x", &only, None).unwrap(), "dall-e-3");
        // No tier matches at all → the catalog's first candidate.
        let none = candidates(&["sdxl", "sd3"]);
        assert_eq!(pick_image_model("x", &none, None).unwrap(), "sdxl");
    }

    #[test]
    fn pick_image_model_case_insensitive_and_empty_candidates_error() {
        let list = candidates(&["GPT-IMAGE-1"]);
        assert_eq!(pick_image_model("x", &list, None).unwrap(), "GPT-IMAGE-1");
        let err = pick_image_model("openai", &[], None).unwrap_err();
        assert!(
            err.contains("provider `openai` has no image-output models"),
            "{err}"
        );
    }

    #[test]
    fn output_format_mime_maps_all_variants() {
        assert_eq!(output_format_mime(OutputFormat::Png), "image/png");
        assert_eq!(output_format_mime(OutputFormat::Jpeg), "image/jpeg");
        assert_eq!(output_format_mime(OutputFormat::Webp), "image/webp");
    }

    #[test]
    fn args_deserialize_and_require_prompt() {
        let args: GenerateImageArgs = serde_json::from_str(r#"{"prompt": "a cat"}"#).unwrap();
        assert_eq!(args.prompt, "a cat");
        assert!(args.model.is_none());
        assert!(args.size.is_none());
        // Missing prompt is a deserialization error (schema `required`).
        assert!(serde_json::from_str::<GenerateImageArgs>(r#"{}"#).is_err());
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

    /// Build a real 4x3 PNG like image.rs's tests do, return its base64.
    fn sample_png_b64() -> String {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(4, 3, |x, y| {
            image::Rgba([x as u8 * 60, y as u8 * 80, 0, 255])
        }));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, ImageFormat::Png).unwrap();
        BASE64.encode(png.into_inner())
    }

    /// Stub client: Debug-able, returns a fixed image generation result.
    #[derive(Debug)]
    struct StubImageClient {
        png_b64: String,
        revised_prompt: Option<String>,
    }

    impl ImageGenerationClient for StubImageClient {
        fn provider_slug(&self) -> &str {
            "openai"
        }
        fn default_image_model(&self) -> &str {
            "gpt-image-1"
        }
        fn generate_image(
            &self,
            _req: &ImageGenerationRequest,
            _cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
        ) -> Result<ImageGenerationResult, InferenceError> {
            Ok(ImageGenerationResult {
                image_b64: self.png_b64.clone(),
                revised_prompt: self.revised_prompt.clone(),
                model: _req.model.clone(),
            })
        }
    }

    /// Run GenerateImage::execute against a mock daemon reply channel that
    /// returns the given handle.
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
        let (daemon_tx, daemon_rx) = mpsc::channel::<crate::daemon::DaemonCommand>();
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
        // revised_prompt lands in the text handle (codex pattern) along with
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
