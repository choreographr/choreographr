//! The shared spine of the `generate_image` and `edit_image` tools.
//!
//! Both tools resolve an image-capable provider through the daemon command
//! loop, pick a catalog-verified model by priority, run the provider
//! round-trip, and then hand the returned bytes through the exact
//! `display_image` prepare pipeline (`prepare_image_from_bytes`). Everything
//! that is identical between the two lives here so the provider resolution,
//! the model tiering, and the decode/validate/return tail cannot drift apart:
//!
//! - `output_format_mime` — the requested-format → MIME mapping.
//! - `pick_image_model` — the catalog-priority model pick.
//! - `resolve_image_provider` — the `GetImageGenerationProvider` round trip.
//! - `sniff_image_mime` — magic-byte MIME detection for the returned bytes.
//! - `finish_provider_image` — the shared decode/prepare/text/return tail.
//!
//! Keeping provider resolution in the daemon command loop means the credential
//! never reaches a tool thread, and re-entering `prepare_image_from_bytes` means
//! the client's display/persistence/vision-feedback path is shared with
//! `display_image` and cannot drift.

use super::image::{DisplayImageReturn, prepare_image_from_bytes};
use super::{PreparedImage, ToolExecError, context::ToolContext, truncate_tool_output};
use crate::providers::ImageProviderHandle;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::images::{ImageGenerationResult, OutputFormat};

/// Map the requested output format to the MIME type the returned bytes are
/// decoded with. Several adapters ignore `output_format` (Google's and xAI's
/// return whatever the model emits, and the fal edit body omits the field at
/// its default), so this mapping is only the FALLBACK used when the returned
/// bytes cannot be sniffed — see [`sniff_image_mime`] and
/// [`finish_provider_image`], which prefer the actual bytes.
pub(crate) fn output_format_mime(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png => "image/png",
        OutputFormat::Jpeg => "image/jpeg",
        OutputFormat::Webp => "image/webp",
    }
}

/// Sniff an image's MIME type from its magic bytes, restricted to the
/// provider-allowlisted raster formats an image-generation response can carry
/// (`image/png`, `image/jpeg`, `image/webp`). Returns `None` for anything else,
/// so a caller can fall back to the requested-format mapping.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => Some("image/png"),
        Ok(image::ImageFormat::Jpeg) => Some("image/jpeg"),
        Ok(image::ImageFormat::WebP) => Some("image/webp"),
        _ => None,
    }
}

/// Pick the image model for a provider.
///
/// `explicit` (the tool's `model` arg) always wins — the model typed the name
/// and the API reports bad names as a clean 4xx, which is the right failure
/// for a deliberate pass-through. Otherwise the choice is made ONLY among
/// catalog-verified candidates (`image_models_for_provider` for generation,
/// `image_edit_models_for_provider` for editing), never guessed: a model that
/// is not in the provider's catalog entry is unlikely to be routable through
/// the account. The priority ordering:
/// gpt-image > imagen > gemini-image > flux > dall-e — "best first" by
/// fidelity/cost behavior, with the legacy dall-e family explicitly last.
///
/// `pub(crate)` so the `edit_image` tool reuses the identical pick over its own
/// edit-capable candidate list (see `tools/image_edit.rs`), rather than
/// duplicating the tiering.
pub(crate) fn pick_image_model(
    slug: &str,
    candidates: &[String],
    explicit: Option<&str>,
) -> Result<String, String> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_string());
    }
    if candidates.is_empty() {
        return Err(format!(
            "provider `{slug}` has no image-output models in the catalog — pass `model` \
             explicitly if your endpoint routes one, or add a `supports_image_output` \
             overlay entry for it and /refresh-models"
        ));
    }

    // Priority tiers by case-insensitive substring match, first match wins.
    // Each candidate is lowercased once per tier comparison — simple and
    // obviously correct; the catalog list is human-catalog-sized, so there
    // is nothing to optimize here.
    let tiers: [(&str, Option<&str>); 5] = [
        ("gpt-image", None),
        ("imagen", None),
        ("gemini-", Some("image")),
        ("flux", None),
        ("dall-e", None),
    ];
    for (primary, secondary) in tiers {
        for candidate in candidates {
            let hay = candidate.to_ascii_lowercase();
            if !hay.contains(primary) {
                continue;
            }
            // "gemini-" + "image": both substrings must be present (a bare
            // gemini chat model would otherwise match the tier).
            if secondary.is_some_and(|sec| !hay.contains(sec)) {
                continue;
            }
            return Ok(candidate.clone());
        }
    }
    // No tier matched: the catalog's first listed image-capable model is the
    // least-wrong default (catalog order is quality-curated upstream).
    // Non-empty per the guard above; fallback is unreachable.
    Ok(candidates.first().cloned().unwrap_or_default())
}

/// Resolve the session's image-capable provider handle through the daemon
/// command loop.
///
/// The command loop is the sole owner of the providers map and the only thread
/// that touches credential state, so the tool sends
/// [`DaemonCommand::GetImageGenerationProvider`](crate::daemon::DaemonCommand::GetImageGenerationProvider)
/// over a fresh crossbeam channel and blocks for the reply. The requesting
/// session's socket registry scopes the built client, so a session cancel
/// closes an in-flight image request too.
pub(crate) fn resolve_image_provider(
    ctx: &ToolContext,
) -> Result<ImageProviderHandle, ToolExecError> {
    let (tx, rx) = crossbeam_channel::unbounded();
    ctx.daemon_tx
        .send(crate::daemon::DaemonCommand::GetImageGenerationProvider {
            // The requesting session scopes the image sockets: the daemon
            // builds the client against THAT session's registry, so a cancel
            // closes an in-flight image request too.
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
    // Map the structured daemon error into its Display text so the model still
    // sees the precise guidance (unlock / account / slug wording).
    handle.map_err(|e| ToolExecError(e.to_string()))
}

/// The shared tail of `generate_image` and `edit_image`: decode the provider's
/// base64 payload, pick the MIME from the ACTUAL bytes, run the shared prepare
/// pipeline, and build the [`DisplayImageReturn`].
///
/// `kind` names the operation in the produced artefacts ("generated" or
/// "edited"): it labels the validation error (`"{kind} image failed
/// validation: …"`) and the text handle (`"{kind} image (…)"`). The MIME is
/// SNIFFED from the bytes ([`sniff_image_mime`]) rather than trusting the
/// requested [`OutputFormat`], because several adapters ignore it — so a
/// `webp`-requested generation that a provider returns as PNG is labelled
/// correctly instead of misreported. `alt` is filtered to a non-empty,
/// non-whitespace value.
///
/// Deliberately does not log: each caller emits its own `info!` with the
/// operation-specific fields (provider slug, model, input/mask stats) read off
/// the returned [`PreparedImage`].
pub(crate) fn finish_provider_image(
    result: &ImageGenerationResult,
    output_format: OutputFormat,
    alt: Option<String>,
    kind: &str,
) -> Result<DisplayImageReturn, ToolExecError> {
    let bytes = BASE64.decode(result.image_b64.trim()).map_err(|e| {
        ToolExecError(format!(
            "provider returned malformed base64 image data: {e}"
        ))
    })?;
    // Prefer the bytes' own magic over the requested format: the request's
    // `output_format` is advisory (see `output_format_mime`), so sniffing is
    // what keeps the label truthful when a provider returns a different
    // encoding than asked for.
    let mime_type = sniff_image_mime(&bytes).unwrap_or(output_format_mime(output_format));
    let (mime_type, width, height) = prepare_image_from_bytes(mime_type, &bytes)
        .map_err(|e| ToolExecError(format!("{kind} image failed validation: {e}")))?;
    let byte_len = bytes.len();

    // Text handle mirrors display_image's, plus the model and the provider's
    // revised prompt — the model benefits from seeing how the provider rewrote
    // its request.
    let revised = result
        .revised_prompt
        .as_deref()
        .map_or_default(|p| format!("\nrevised prompt: {p}"));
    let text = truncate_tool_output(&format!(
        "{kind} image ({mime_type}, {width}x{height}, {}) via {model}{revised}",
        humfmt::bytes(byte_len as u64),
        model = result.model,
    ));

    Ok(DisplayImageReturn {
        text,
        image: PreparedImage {
            mime_type,
            data: bytes,
            width,
            height,
            alt: alt.filter(|alt| !alt.trim().is_empty()),
        },
    })
}

/// Build a real 4x3 RGBA PNG and return its base64 — valid, provider-shaped
/// bytes shared by the image-tool unit tests.
#[cfg(test)]
pub(crate) fn sample_png_b64() -> String {
    use image::ImageFormat;
    use std::io::Cursor;
    let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(4, 3, |x, y| {
        // u8 pixel coordinates (4×3 image): the arithmetic never exceeds u8.
        #[expect(clippy::cast_possible_truncation)]
        image::Rgba([x as u8 * 60, y as u8 * 80, 0, 255])
    }));
    let mut png = Cursor::new(Vec::new());
    img.write_to(&mut png, ImageFormat::Png).unwrap();
    BASE64.encode(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(list: &[&str]) -> Vec<String> {
        list.iter().map(std::string::ToString::to_string).collect()
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
    fn sniff_image_mime_recognizes_the_allowlisted_formats() {
        // Real encoded bytes of the three allowlisted formats.
        let png = BASE64
            .decode(sample_png_b64())
            .expect("sample png base64 decodes");
        assert_eq!(sniff_image_mime(&png), Some("image/png"));

        let rgba = image::RgbaImage::from_fn(2, 2, |_, _| image::Rgba([10, 20, 30, 255]));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba.clone())
            .to_rgb8()
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("encode jpeg");
        assert_eq!(sniff_image_mime(&jpeg.into_inner()), Some("image/jpeg"));

        // Anything else (unsniffable / not allowlisted) returns None.
        assert_eq!(sniff_image_mime(&[0xDE, 0xAD, 0xBE, 0xEF]), None);
        assert_eq!(sniff_image_mime(b"not an image"), None);
    }
}
