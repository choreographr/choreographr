//! The opencode.ai zen/go gateway's transient message-content 400 policy.
//!
//! The gateway validates message *content* on only some of its routes, so the
//! identical request bytes flip 200/400/200 there.  This submodule owns the
//! three pieces that make a bounded resend of that flake safe: the resend count
//! ([`GATEWAY_TRANSIENT_400_ATTEMPTS`]), the signature gate
//! ([`is_transient_gateway_400`]), and the error-code recovery used to label a
//! retry in the log ([`gateway_error_code`]).
//!
//! The driver that consumes them lives in `retry_loop_with_provider_policies`
//! (in the parent module), mirroring how the 415 compression downgrade's driver
//! lives there while its supporting policy lives in [`super::compression`].

use crate::shared::is_opencode_gateway_slug;

/// Bounded number of extra resends for a transient opencode-gateway 400 (see
/// [`super::retry_loop_with_provider_policies`]). Two resends cover the observed
/// route-flap — the same order the community retry work-around uses — while a
/// genuinely deterministic rejection is only delayed by a few seconds before it
/// surfaces.
pub(super) const GATEWAY_TRANSIENT_400_ATTEMPTS: u32 = 2;

/// Extract the provider/gateway error code that the opencode.ai zen/go gateway
/// preserves as a `[code]` label at the start of a rewritten error message,
/// e.g. `Upstream request failed: [unsupported_request] Message content must …`
/// → `Some("unsupported_request")`. The gateway drops upstream codes outside
/// its allow-list but keeps the original in this bracketed label; recovering it
/// is what makes the failure class diagnosable — the retry's warn line labels
/// the retry with it — without dumping the human-readable prose into the log.
///
/// Mirrors the gateway's own `<colon- and newline-free prefix>: [<code>]` shape:
/// the prefix before the first colon must be non-empty and single-line, and the
/// code must be `[A-Za-z0-9_.-]+`. Returns `None` when the message is not in
/// that shape (a non-gateway error, or an opaque body with no label).
#[must_use]
pub(super) fn gateway_error_code(message: &str) -> Option<&str> {
    let colon = message.find(':')?;
    let prefix = message.get(..colon)?;
    if prefix.is_empty() || prefix.contains('\n') {
        return None;
    }
    let rest = message.get(colon + 1..)?.strip_prefix(" [")?;
    let code = rest.get(..rest.find(']')?)?;
    (!code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    .then_some(code)
}

/// Whether a terminal `400` from `slug` is one of the opencode.ai zen/go
/// gateway's *transient* message-content rejections — the ones it enforces on
/// only some routes, so the identical request bytes often succeed on a resend.
///
/// Deliberately narrow, in three independent gates so nothing unrelated is ever
/// retried: the slug must be a gateway slug (see [`is_opencode_gateway_slug`]),
/// and the detail must either carry one of the gateway's exact content
/// wordings or be the opaque model-only stub (`{"model":"…"}`, no
/// `error`/`message` field) that upstream measured as route-flaky. Any other
/// `400` — a genuine content/shape rejection, auth, quota, or a non-gateway
/// provider's error — returns `false` and stays terminal.
///
/// The gate matches the human-readable wordings, not the recovered `[code]`
/// label: the label is generic (`unsupported_request`) and would also cover
/// deterministic rejections, so the wording is the specific signal. The label
/// is recovered separately ([`gateway_error_code`]) only to label the log line.
pub(super) fn is_transient_gateway_400(slug: &str, detail: &str) -> bool {
    // Both gateway content validators: text required, or text only (media not
    // enabled). Matched as substrings so the surrounding prefix does not matter.
    const MARKERS: &[&str] = &[
        "Message content must contain text",
        "Message content supports text content only",
    ];
    if !is_opencode_gateway_slug(slug) {
        return false;
    }
    if MARKERS.iter().any(|marker| detail.contains(marker)) {
        return true;
    }
    // Opaque model-only 400 stub: the body carried no reason at all, and
    // upstream measured it as route-flaky (byte-identical 200/400/200).
    detail
        .strip_prefix("request failed with status 400:")
        .map(str::trim)
        .is_some_and(|body| {
            body.starts_with('{')
                && body.contains("\"model\"")
                && !body.contains("\"error\"")
                && !body.contains("\"message\"")
        })
}
