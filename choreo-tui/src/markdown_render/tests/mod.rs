mod ansi;
mod blockquote;
mod code_box;
mod copy_metadata;
mod incremental;
mod inline;
mod lists;
mod math;
mod punctuation;
mod reasoning;
mod syntax;
mod tables;
mod text;
mod tool_results;
mod tools;
mod turn;

/// Run `check` over every Unicode scalar value, sharded across the available
/// cores. The code-space sweep below is exhaustive by design (it pins the TUI's
/// per-char terminal keep policy against the shared spoofing predicate for
/// *every* char) and pure CPU; sharding keeps it exhaustive while cutting its
/// wall time to roughly one core's share. A panic in any shard propagates out
/// of `scope` once every thread has joined.
pub(super) fn sweep_code_space(check: impl Fn(char) + Sync) {
    const TOTAL: u32 = 0x11_0000; // Unicode scalar range, excluding surrogates.
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let shard = TOTAL.div_ceil(u32::try_from(threads).unwrap_or(1)).max(1);
    std::thread::scope(|scope| {
        let mut start = 0u32;
        while start < TOTAL {
            let end = (start + shard).min(TOTAL);
            let check = &check;
            scope.spawn(move || {
                for cp in start..end {
                    if let Some(c) = char::from_u32(cp) {
                        check(c);
                    }
                }
            });
            start = end;
        }
    });
}

/// Number of leading space characters in a rendered line (0 when it does
/// not start with spaces).
pub(super) fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|ch| *ch == ' ').count()
}

/// Column (byte index, ASCII-only test input) where the first non-marker
/// text of a rendered list line begins — i.e. where the content starts.
pub(super) fn first_content_column(line: &str) -> usize {
    line.char_indices()
        .find(|(_, ch)| ch.is_alphabetic())
        .map_or(0, |(idx, _)| idx)
}
