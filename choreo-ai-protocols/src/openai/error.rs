//! The model-usage error annotation.
//!
//! The live model picker deliberately lists the provider's *full* catalogue,
//! so a turn can be dispatched at a model that cannot be driven (`OpenAI`'s
//! legacy completions models, embeddings, audio, image, moderation). The
//! provider is the only reliable authority on usability — its `/models`
//! payload carries no modality — so this module reacts to its verdict and
//! rewrites it into one short user-facing line, keeping the provider's raw
//! wording for operators.

use tracing::info;

use super::OpenAiError;

/// Rewrite a provider rejection that means "this model can't be used here"
/// into a short, plain message, e.g. `'gpt-4o' is not a chat model — please
/// try a different one.`
///
/// A just-released or custom model must stay selectable, so the list is never
/// filtered; instead, when a turn is sent with an unusable model, the
/// provider's verdict (e.g. the `OpenAI` `404 This is not a chat model …`)
/// becomes a short user-facing line. The provider's raw wording is jargon to a
/// user, so it is dropped from the message and kept in the log instead.
pub(crate) fn annotate_model_usage_error(model: &str, err: OpenAiError) -> OpenAiError {
    let OpenAiError::ClientError { status, detail } = &err else {
        return err;
    };
    // The helper lowercases once and decides both *whether* this is a
    // model-usability rejection and *how* to phrase it; a `None` here means
    // the body is unrelated and must pass through verbatim.
    let Some(reason) = model_usage_rejection_reason(detail) else {
        return err;
    };
    // Keep the provider's technical wording for operators — the user only sees
    // the plain line below.
    info!(model = %model, provider_detail = %detail, "provider rejected the model as unusable");
    OpenAiError::ClientError {
        status: *status,
        detail: format!("'{model}' {reason} — please try a different one."),
    }
}

/// Classify a provider error body: `Some(reason)` when it means the selected
/// model cannot be used on the endpoint, with the short phrase to name the
/// cause; `None` when it is unrelated (and must pass through verbatim). The
/// body is free-form prose, so this is a deliberately narrow substring match
/// over the known OpenAI-family rejections.
///
/// `crate::retry::extract_error_message` (in `retry.rs`) unwraps the standard
/// JSON error envelope and returns only `error.message`, DROPPING the `code`
/// field. On that parsed-envelope path the human-readable
/// `"does not exist or you do not have access"` prose is therefore the primary
/// model-not-found signal — the literal `"model_not_found"` code never
/// survives extraction. That literal only reaches here when the body was NOT a
/// parseable envelope, in which case the verbatim body is kept.
///
/// Note: `"not supported in the v1/chat/completions"` / `"not supported in the
/// v1/responses"` are deliberately NOT matched — `OpenAI` uses the same wording
/// for genuine "Unsupported parameter" errors, so matching them would mask real
/// parameter bugs behind the generic line. The intended legacy-completions case
/// is already covered by `"not a chat model"`.
pub(crate) fn model_usage_rejection_reason(detail: &str) -> Option<&'static str> {
    // One lowercase pass for both the classification and the phrasing.
    let d = detail.to_ascii_lowercase();
    if d.contains("not a chat model") {
        Some("is not a chat model")
    } else if d.contains("model_not_found")
        || d.contains("does not exist or you do not have access")
    {
        Some("can't be used here")
    } else {
        None
    }
}
