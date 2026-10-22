//! Value formatting shared by the status bar and the session-manager detail
//! pane: timestamps and the two cumulative-token readouts.
//!
//! Every counter here goes through humfmt's compact number formatter so the
//! token surfaces agree with each other and with the context-window fill drawn
//! beside them (which uses `humfmt::number`/`humfmt::percent`): small values
//! render verbatim, large ones gain K/M suffixes.
//!
//! [`format_timestamp`] takes Unix **milliseconds**, not seconds, and always
//! produces ASCII — callers use its width to reserve columns, so a localized or
//! multi-byte rendering would misalign the row it labels.

use choreo_proto::TokenUsage;
use std::fmt::Write as _;

/// Format a Unix-epoch-milliseconds timestamp as simplified absolute time.
///
/// - today → "14:32" (time only)
/// - this calendar year → "Mar 5"
/// - older → "Mar 5 2024"
pub(crate) fn format_timestamp(ts_ms: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};

    if ts_ms <= 0 {
        return "-".to_string();
    }

    let chrono::LocalResult::Single(dt) = Local.timestamp_millis_opt(ts_ms) else {
        return "-".to_string();
    };

    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %d").to_string()
    } else {
        dt.format("%b %d %Y").to_string()
    }
}

/// The status bar's cumulative token readout, e.g. `↑15.3K ↓1.2K`.
///
/// Both counters pass through humfmt's compact number formatter so the
/// readout stays consistent with the context-window fill rendered beside it
/// (which already uses `humfmt::number`/`humfmt::percent`): small sessions
/// (< `1_000` tokens) render verbatim, large ones get K/M suffixes.
pub(crate) fn status_token_readout(usage: &TokenUsage) -> String {
    format!(
        "↑{} ↓{}",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
    )
}

/// The session-detail "Tokens:" line, e.g.
/// `Tokens:        15.3K in / 1.2K out (16.5K total)`.
///
/// Same humfmt treatment as the status bar's readout so the two token
/// surfaces agree; the `Tokens:        ` label keeps the column aligned with
/// its neighbours (`Working Dir:`, `Turn Count:`, …).
pub(crate) fn session_detail_tokens_line(usage: &TokenUsage) -> String {
    let mut line = format!(
        "Tokens:        {} in / {} out ({} total)",
        humfmt::number(usage.input_tokens),
        humfmt::number(usage.output_tokens),
        humfmt::number(usage.total_tokens),
    );
    // Cached prompt tokens are a subset of the input count; annotate only
    // when the provider actually reported them so the common case is unchanged.
    if usage.cached_tokens > 0 {
        // `String`'s `fmt::Write` impl is infallible; the `Result` is
        // discarded on purpose (write! to a String can never fail).
        let _ = write!(line, " ({} cached)", humfmt::number(usage.cached_tokens));
    }
    line
}
