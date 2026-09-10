//! Zhipu (z.ai / bigmodel.cn) provider-specific request-shaping logic for the
//! OpenAI-compatible chat adapter.
//!
//! Kept in a dedicated submodule (rather than inlined into `openai/mod.rs`) so
//! the provider-specific mapping stays out of the general dispatch surface —
//! `mod.rs` only re-exports the two entry points below.

use tracing::{debug, warn};

/// Whether a catalog provider slug is one of the two Zhipu chat slugs the
/// chat adapter must apply z.ai's model-specific `reasoning_effort` mapping
/// to (see [`zhipu_reasoning_effort_api_value`]):
///
/// - `"zai"` — the z.ai PaaS gateway,
/// - `"zhipuai"` — the mainland bigmodel endpoint.
///
/// This mirrors `images::is_zhipu_image_provider_slug`, which exists for the
/// image dispatch; the two are kept textually in sync rather than shared so
/// the chat path never depends on the images module. If a third Zhipu slug
/// appears, both allowlists must gain it together.
pub(crate) fn is_zhipu_provider_slug(slug: &str) -> bool {
    matches!(slug, "zai" | "zhipuai")
}

/// Map our OpenAI-style reasoning-effort slug to z.ai's documented
/// `reasoning_effort` wire value for the given model (chat-completions
/// API, docs.z.ai — POST /paas/v4/chat/completions).
///
/// z.ai's API does not accept the full OpenAI effort set, and the accepted
/// values differ by model generation:
///
/// - **GLM-5.3 / GLM-5.3-flash** accept ONLY `low` / `high` / `max`, and
///   thinking can never be disabled (`thinking.type` is fixed to
///   `enabled`). Unsupported slugs are therefore coerced to the nearest
///   supported level, consistent with the documented 5.2 family mappings.
/// - **GLM-5.2 and below** follow the documented family mappings: `none` /
///   `minimal` skip thinking, `low` and `medium` are mapped to `high`, and
///   `xhigh` is mapped to `max` (the documented default).
///
/// `off` always maps to `None` (field omitted) — for 5.2-and-below that is
/// the provider-documented way to skip thinking; for 5.3 thinking stays on
/// at its `max` default regardless, which the caller warns about.
pub(crate) fn zhipu_reasoning_effort_api_value<'a>(slug: &'a str, model: &str) -> Option<&'a str> {
    // Deliberately narrow name check: only the 5.3 generation tightened its
    // accepted effort set, and 5.3-specific mappings must not leak onto
    // other GLM models (e.g. a future "glm-5.3x" variant would still want
    // this, but "glm-5.2" must not).
    let is_glm_5_3 = model.to_ascii_lowercase().starts_with("glm-5.3");

    let mapped = match slug {
        // Omitted entirely: GLM-5.3 cannot disable thinking (defaults to
        // `max`), GLM-5.2-and-below skip thinking — both are the
        // documented behaviors for an absent field.
        "off" => {
            if is_glm_5_3 {
                warn!(
                    model = %model,
                    "GLM-5.3 cannot disable thinking; request sent without \
                     reasoning_effort and the model will think at its `max` default"
                );
            }
            None
        }
        // Every supported-effort slug folds through one coercion table and a
        // SINGLE warn site: a warn fires only when the wire value differs
        // from the requested slug. GLM-5.3 folds everything below `high`
        // into `low`; the 5.2 family documents `minimal` as skip-thinking,
        // `low`/`medium`/`high` as `high`, and `xhigh`/`max` as `max` (the
        // documented default). `max` passes through on both generations.
        slug @ ("minimal" | "low" | "medium" | "high" | "xhigh" | "max") => {
            let value = if is_glm_5_3 {
                match slug {
                    "minimal" | "low" => "low",
                    "medium" | "high" => "high",
                    _ => "max",
                }
            } else {
                match slug {
                    "minimal" => "minimal",
                    "low" | "medium" | "high" => "high",
                    _ => "max",
                }
            };
            if value != slug {
                warn!(
                    model = %model,
                    requested = %slug,
                    mapped = %value,
                    "coercing unsupported reasoning_effort to z.ai-documented value"
                );
            }
            Some(value)
        }
        // Unknown slug: pass through untouched so new upstream effort
        // levels are not silently dropped; the API will reject an invalid
        // value loudly rather than us guessing a coercion.
        other => {
            debug!(
                model = %model,
                slug = %other,
                "unrecognized reasoning effort slug; passing through to z.ai unchanged"
            );
            Some(other)
        }
    };

    debug!(
        model = %model,
        slug = %slug,
        ?mapped,
        "zhipu reasoning_effort mapping"
    );
    mapped
}

#[cfg(test)]
mod tests {
    use super::super::chat_completions::ChatCompletionsRequest;
    use super::*;
    use crate::openai::ChatRequestMessage;

    #[test]
    fn zhipu_reasoning_effort_mapper_table() {
        // GLM-5.3: only low/high/max are accepted on the wire; off is
        // omitted (thinking stays on at max — unavoidable for 5.3).
        for model in ["glm-5.3", "glm-5.3-flash", "GLM-5.3-FLASH"] {
            assert_eq!(zhipu_reasoning_effort_api_value("off", model), None);
            assert_eq!(
                zhipu_reasoning_effort_api_value("minimal", model),
                Some("low")
            );
            assert_eq!(zhipu_reasoning_effort_api_value("low", model), Some("low"));
            assert_eq!(
                zhipu_reasoning_effort_api_value("medium", model),
                Some("high")
            );
            assert_eq!(
                zhipu_reasoning_effort_api_value("high", model),
                Some("high")
            );
            assert_eq!(
                zhipu_reasoning_effort_api_value("xhigh", model),
                Some("max")
            );
            assert_eq!(zhipu_reasoning_effort_api_value("max", model), Some("max"));
        }

        // GLM-5.2 and below: documented family mappings (minimal skips
        // thinking, low/medium → high, xhigh → max, max passes through).
        for model in ["glm-5.2", "glm-5.1", "glm-5", "glm-4.7"] {
            assert_eq!(zhipu_reasoning_effort_api_value("off", model), None);
            assert_eq!(
                zhipu_reasoning_effort_api_value("minimal", model),
                Some("minimal")
            );
            assert_eq!(zhipu_reasoning_effort_api_value("low", model), Some("high"));
            assert_eq!(
                zhipu_reasoning_effort_api_value("medium", model),
                Some("high")
            );
            assert_eq!(
                zhipu_reasoning_effort_api_value("high", model),
                Some("high")
            );
            assert_eq!(
                zhipu_reasoning_effort_api_value("xhigh", model),
                Some("max")
            );
            assert_eq!(zhipu_reasoning_effort_api_value("max", model), Some("max"));
        }

        // Unknown slug passes through unchanged (no silent coercion) for
        // both GLM generations.
        assert_eq!(
            zhipu_reasoning_effort_api_value("turbo", "glm-5.3"),
            Some("turbo")
        );
        assert_eq!(
            zhipu_reasoning_effort_api_value("turbo", "glm-5.2"),
            Some("turbo")
        );
    }

    #[test]
    fn zhipu_slug_allowlist_matches_images_helper() {
        // The chat-path allowlist must stay in sync with the images
        // module's dispatch helper — both cover exactly zai + zhipuai.
        for slug in ["zai", "zhipuai"] {
            assert!(is_zhipu_provider_slug(slug));
            assert!(crate::images::is_zhipu_image_provider_slug(slug));
        }
        for slug in ["openai", "zhipu", "", "zai-coding-plan"] {
            assert!(!is_zhipu_provider_slug(slug));
            assert!(!crate::images::is_zhipu_image_provider_slug(slug));
        }
    }

    #[test]
    fn chat_body_carries_mapped_reasoning_effort_for_glm_5_3() {
        // End-to-end wire shape: a glm-5.3 request under the zai slug with
        // our `medium` slug must serialize `reasoning_effort: "high"` in
        // the chat-completions body (z.ai only accepts low/high/max there).
        assert!(is_zhipu_provider_slug("zai"));
        let effort = zhipu_reasoning_effort_api_value("medium", "glm-5.3");
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "glm-5.3",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: effort,
        })
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }
}
