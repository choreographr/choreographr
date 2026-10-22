use crate::render::*;
use choreo_proto::{SessionStatus, TokenUsage};

// ── format_status tests ──

#[test]
fn format_status_retrying() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 5,
        delay_ms: 3000,
    };
    assert_eq!(format_status(&status), "retrying (2/5, 3s)");
}

#[test]
fn format_status_retrying_first_attempt() {
    let status = SessionStatus::Retrying {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1500,
    };
    assert_eq!(format_status(&status), "retrying (1/3, 1s 500ms)");
}

#[test]
fn format_status_retrying_second() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 2000,
    };
    assert_eq!(format_status(&status), "retrying (2/3, 2s)");
}

// ── Cumulative token readouts (status bar + session detail) ──

#[test]
fn status_token_readout_small_counts_pass_through() {
    // Below the 1_000 compact threshold humfmt renders verbatim, so small
    // sessions read exactly like the raw counts used to.
    let usage = TokenUsage {
        input_tokens: 847,
        output_tokens: 23,
        total_tokens: 870,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(status_token_readout(&usage), "↑847 ↓23");
}

#[test]
fn status_token_readout_compacts_large_counts() {
    // Above the threshold humfmt compacts with K/M suffixes, matching the
    // context-fill readout beside it (e.g. "1.5K / 32K").  Values chosen
    // from humfmt's documented outputs: 15_320 -> "15.3K", 1_280 -> "1.3K"
    // (HalfUp at precision 1, trailing zero trimmed).
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(status_token_readout(&usage), "↑15.3K ↓1.3K");
}

#[test]
fn status_token_readout_zero() {
    assert_eq!(status_token_readout(&TokenUsage::default()), "↑0 ↓0");
}

#[test]
fn session_detail_tokens_line_compacts_and_keeps_label_alignment() {
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        15.3K in / 1.3K out (16.6K total)"
    );
}

#[test]
fn session_detail_tokens_line_annotates_cached_tokens_when_reported() {
    // z.ai reports cached prompt tokens; the detail line annotates them as a
    // subset of the input count. Zero cached stays silent (see the test above).
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
        cached_tokens: 10_000,
        cache_write_tokens: 0,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        15.3K in / 1.3K out (16.6K total) (10K cached)"
    );
}

#[test]
fn session_detail_tokens_line_small_counts_pass_through() {
    let usage = TokenUsage {
        input_tokens: 42,
        output_tokens: 7,
        total_tokens: 49,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        42 in / 7 out (49 total)"
    );
}

// ── format_timestamp ──────────────────────────────────────────────────

#[test]
fn format_timestamp_zero_or_negative_is_dash() {
    assert_eq!(format_timestamp(0), "-");
    assert_eq!(format_timestamp(-5), "-");
}

#[test]
fn format_timestamp_today_shows_time_only() {
    use chrono::Local;
    let now = Local::now();
    let s = format_timestamp(now.timestamp_millis());
    assert!(
        s.len() == 5 && s.contains(':'),
        "today renders as HH:MM, got {s}"
    );
}

#[test]
fn format_timestamp_same_year_shows_month_and_day() {
    use chrono::{Datelike, Local, TimeZone};
    let now = Local::now();
    // A fixed mid-year date is always in the current calendar year; if it
    // happens to be today, nudge to a neighbouring date that still can't be
    // today and is guaranteed to stay in the same year.
    let mut dt = Local
        .with_ymd_and_hms(now.year(), 6, 15, 12, 0, 0)
        .single()
        .expect("valid date");
    if dt.date_naive() == now.date_naive() {
        dt = Local
            .with_ymd_and_hms(now.year(), 6, 16, 12, 0, 0)
            .single()
            .expect("valid date");
    }
    let s = format_timestamp(dt.timestamp_millis());
    assert_eq!(
        s,
        dt.format("%b %d").to_string(),
        "same-year date shows month+day"
    );
    assert!(!s.contains(':'), "no time for non-today same-year dates");
}

#[test]
fn format_timestamp_older_year_shows_full_date() {
    use chrono::{Datelike, Local, TimeZone};
    let year = Local::now().year() - 2;
    let dt = Local
        .with_ymd_and_hms(year, 6, 15, 12, 0, 0)
        .single()
        .expect("valid date");
    let s = format_timestamp(dt.timestamp_millis());
    assert_eq!(
        s,
        dt.format("%b %d %Y").to_string(),
        "older dates include the year"
    );
}
