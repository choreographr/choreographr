//! The pure machinery the two picker popups share — the wizard's provider
//! picker and the model selector: the filter predicate ([`filter_matches`]),
//! the render-window arithmetic ([`picker_window`]) and the pin-at-middle
//! arrow/wheel navigation ([`step_focus`]).
//!
//! They are free functions rather than methods because both pickers carry their
//! own `focused`/`scroll` fields but must agree on the rules; keeping the rules
//! in one place is what stops one picker's scrolling from drifting away from
//! the other's.

/// The two pickers' shared case-insensitive substring predicate: does `s`
/// contain the already-lowercased `needle`?  Both `filtered()` methods and
/// their `filtered_len()` count helpers apply the same predicate, so a
/// filter change can never behave differently between rendering and
/// navigation.
pub(in crate::state) fn filter_matches(s: &str, needle: &str) -> bool {
    s.to_lowercase().contains(needle)
}

/// Shared pure window arithmetic for the two picker popups (the wizard's
/// provider picker and the model selector): compute the `(start, count)` slice
/// of a `len`-item list to render in a window of `height` rows, keeping the
/// `focused` row visible.  `scroll` is a hint only and is corrected locally
/// (clamped to the valid range, pulled up when focus drifts above the window,
/// pushed down when focus falls below the fold), so repeated calls with the
/// same inputs return identical results — render can never mutate focus/scroll
/// state during `draw()`.
pub(crate) fn picker_window(
    scroll: usize,
    focused: usize,
    len: usize,
    height: usize,
) -> (usize, usize) {
    if len == 0 || height == 0 {
        return (0, 0);
    }
    let focused = focused.min(len - 1);
    let max_scroll = len.saturating_sub(height);
    let mut scroll = scroll.min(max_scroll);
    if focused < scroll {
        // Focus drifted above the window (e.g. after a filter that shrunk
        // the list) — pull the window up.
        scroll = focused;
    } else if focused >= scroll + height {
        // Focus is below the fold — push the window down.
        scroll = focused + 1 - height;
    }
    (scroll, height.min(len - scroll))
}

/// Shared pin-at-middle arrow/wheel navigation for the two picker popups (the
/// wizard's provider picker and the model selector): move the highlight one
/// row toward the top/bottom of a `len`-item list rendered in a window of
/// `height` rows, returning the new `(focused, scroll)` pair.
///
/// ↓ walks the highlight down through a static window; once it reaches the
/// middle row (`height / 2`) further presses keep it pinned there while
/// `scroll` increments — the list slides under the highlight.  At the bottom
/// edge (`scroll == max_scroll = len.saturating_sub(height)`) it un-pins and
/// the highlight walks the rest of the way to the last item.  ↑ is the exact
/// mirror: pinned at the middle while `scroll > 0` (decrementing `scroll`),
/// un-pinned at `scroll == 0` to walk up to row 0.  When `height == 0`
/// (viewport unknown — 0 until the first frame) it falls back to focus-only
/// moves with the render-time `picker_window` clamp, preserving the original
/// navigation; when the list fits the window (`len <= height`, so
/// `max_scroll == 0`) the plain `picker_window` behavior applies (highlight
/// walks edge to edge, window static).  `focused`/`scroll` are hints only — a
/// stale pair is clamped locally, exactly like `picker_window` (`scroll` to
/// the true `max_scroll`, symmetrically for both directions).
pub(in crate::state) fn step_focus(
    focused: usize,
    scroll: usize,
    len: usize,
    height: usize,
    down: bool,
) -> (usize, usize) {
    if len == 0 {
        // Nothing to focus: reset the pair so a stale highlight cannot linger
        // on a list the filter just emptied.
        return (0, 0);
    }
    if height == 0 {
        // Viewport unknown: focus-only moves, `picker_window` keeps the
        // highlight visible at render time.
        return if down {
            if focused + 1 < len {
                (focused + 1, scroll)
            } else {
                (focused, scroll)
            }
        } else if focused > 0 {
            (focused - 1, scroll)
        } else {
            (focused, scroll)
        };
    }
    // Clamp a stale pair before stepping (a filter can narrow the list under
    // a highlight that pointed past the end; `clamp_focus` normally does this
    // at mutation time, this is the pure-function safety net).  `scroll` is
    // clamped to the true max_scroll *symmetrically* for both directions: a
    // hint left past the bottom edge must not make ↑ decrement it one press
    // at a time while ↓ clamps it in one step — both treat the window as
    // already pinned at the edge the hint points past.
    let focused = focused.min(len - 1);
    let max_scroll = len.saturating_sub(height);
    let scroll = scroll.min(max_scroll);
    let middle = height / 2;
    if down {
        if focused + 1 >= len {
            // Already at the last item.
            return (focused, scroll);
        }
        if scroll >= max_scroll {
            // Window pinned at the bottom edge: un-pin and walk the highlight
            // down (clamping a stale scroll hint to the valid range).
            (focused + 1, max_scroll)
        } else if focused < scroll + middle {
            // Highlight above the middle row: walk down inside the static
            // window.
            (focused + 1, scroll)
        } else {
            // Pinned at (or below) the middle row: the list slides under the
            // highlight, which stays at the same visual offset.
            (focused + 1, scroll + 1)
        }
    } else {
        if focused == 0 {
            // Already at the first item.
            return (focused, scroll);
        }
        if scroll == 0 {
            // Window pinned at the top edge: un-pin and walk the highlight up.
            (focused - 1, scroll)
        } else if focused > scroll + middle {
            // Highlight below the middle row: walk up inside the static
            // window.
            (focused - 1, scroll)
        } else {
            // Pinned at (or above) the middle row: the list slides under the
            // highlight, which stays at the same visual offset.
            (focused - 1, scroll - 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{picker_window, step_focus};
    use crate::state::ModelSelectorState;

    #[test]
    fn picker_window_empty_list_or_zero_height_returns_zero() {
        // No rows to show and/or no room to show them: the window is empty.
        assert_eq!(picker_window(0, 0, 0, 10), (0, 0), "empty list");
        assert_eq!(picker_window(0, 0, 5, 0), (0, 0), "zero height");
    }

    #[test]
    fn picker_window_focus_below_the_fold_pushes_window_down() {
        // 5 rows in a 3-row window: focus 4 must be the last visible row.
        assert_eq!(picker_window(0, 4, 5, 3), (2, 3));
    }

    #[test]
    fn picker_window_focus_above_the_window_pulls_it_up() {
        // A stale scroll hint points past the focused row — the window snaps
        // up so the focus is the first visible row.
        assert_eq!(picker_window(4, 1, 5, 3), (1, 3));
    }

    #[test]
    fn picker_window_stale_scroll_hint_is_clamped() {
        // `scroll` is a hint only: past the valid range it is clamped locally
        // (the stored field is never written back by render).
        assert_eq!(picker_window(10, 4, 5, 3), (2, 3));
    }

    #[test]
    fn picker_window_count_never_exceeds_the_list_tail() {
        // A window taller than the list renders the whole list.
        assert_eq!(picker_window(0, 0, 2, 10), (0, 2));
        assert_eq!(picker_window(0, 1, 2, 10), (0, 2));
    }

    #[test]
    fn picker_window_focus_is_always_inside_the_window() {
        // The invariant the two pickers rely on: the focused row is always
        // visible for any (scroll, focused, len, height) combination.
        for &(scroll, focused, len, height) in &[
            (0, 0, 1, 1),
            (3, 4, 5, 3),
            (1, 4, 5, 3),
            (2, 0, 5, 3),
            (4, 4, 5, 3),
            (9, 9, 10, 4),
        ] {
            let (start, count) = picker_window(scroll, focused, len, height);
            assert!(count > 0, "non-empty list must render rows");
            assert!(
                (start..start + count).contains(&focused.min(len - 1)),
                "focus must stay visible for ({scroll}, {focused}, {len}, {height})"
            );
        }
    }

    // ── step_focus (pin-at-middle arrow/wheel navigation) ───────────────

    #[test]
    fn step_focus_pins_at_middle_going_down() {
        // 20 items in a 10-row window: the middle row is 5.  The first five
        // ↓ presses walk the highlight down through the static window…
        let mut focused = 0usize;
        let mut scroll = 0usize;
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (5, 0),
            "highlight reaches the middle row"
        );

        // …the next five keep it pinned there while the list slides under it
        // (focused − scroll stays 5).
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (10, 5),
            "pinned at the middle while scrolling"
        );

        // Still pinned when scroll reaches max_scroll (20 − 10 = 10).
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!(
            (focused, scroll),
            (15, 10),
            "max_scroll reached, still pinned"
        );

        // At the bottom edge it un-pins and walks to the last item.
        for _ in 0..4 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!((focused, scroll), (19, 10), "walked to the last item");

        // Further presses are no-ops.
        (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        assert_eq!((focused, scroll), (19, 10), "no-op at the last item");
    }

    #[test]
    fn step_focus_un_pins_at_bottom_edge() {
        // When the window is already pinned at max_scroll, ↓ moves the
        // highlight without scrolling — walking from the middle row to the
        // last item.
        let mut focused = 5usize;
        let mut scroll = 10usize; // max_scroll for 20 items in 10 rows
        for _ in 0..14 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, true);
        }
        assert_eq!((focused, scroll), (19, 10));
    }

    #[test]
    fn step_focus_mirror_going_up() {
        // Mirror of the down walk: start at the last item with the window at
        // max_scroll.  The first presses walk the highlight up through the
        // static window to the middle…
        let mut focused = 19usize;
        let mut scroll = 10usize;
        for _ in 0..4 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!((focused, scroll), (15, 10), "walked up to the middle row");

        // …then it pins at the middle while the list scrolls up until
        // scroll reaches 0…
        for _ in 0..10 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!(
            (focused, scroll),
            (5, 0),
            "pinned at the middle while scrolling up"
        );

        // …and finally un-pins at the top edge to walk to row 0.
        for _ in 0..5 {
            (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        }
        assert_eq!((focused, scroll), (0, 0), "walked to the first row");

        // Further presses are no-ops.
        (focused, scroll) = step_focus(focused, scroll, 20, 10, false);
        assert_eq!((focused, scroll), (0, 0), "no-op at the first row");
    }

    #[test]
    fn step_focus_list_fits_in_window() {
        // 3 items in a 10-row window: max_scroll == 0, so the plain
        // picker_window behavior applies — the highlight walks edge to edge
        // while the window stays at row 0.
        assert_eq!(step_focus(0, 0, 3, 10, true), (1, 0));
        assert_eq!(step_focus(1, 0, 3, 10, true), (2, 0));
        assert_eq!(
            step_focus(2, 0, 3, 10, true),
            (2, 0),
            "no-op at the last item"
        );

        assert_eq!(step_focus(2, 0, 3, 10, false), (1, 0));
        assert_eq!(step_focus(1, 0, 3, 10, false), (0, 0));
        assert_eq!(
            step_focus(0, 0, 3, 10, false),
            (0, 0),
            "no-op at the first item"
        );
    }

    #[test]
    fn step_focus_height_zero_falls_back_to_focus_only() {
        // Viewport unknown (0 until the first frame): focus-only moves with
        // the render-time clamp — scroll is never touched.
        assert_eq!(step_focus(0, 7, 20, 0, true), (1, 7));
        assert_eq!(step_focus(5, 7, 20, 0, false), (4, 7));
        assert_eq!(step_focus(0, 7, 20, 0, false), (0, 7), "no-op at the top");
        assert_eq!(
            step_focus(19, 7, 20, 0, true),
            (19, 7),
            "no-op at the bottom"
        );
    }

    #[test]
    fn step_focus_height_one() {
        // A 1-row window pins at row 0 (height / 2): every press scrolls by
        // one row.
        assert_eq!(step_focus(0, 0, 3, 1, true), (1, 1));
        assert_eq!(step_focus(1, 1, 3, 1, true), (2, 2));
        assert_eq!(
            step_focus(2, 2, 3, 1, true),
            (2, 2),
            "no-op at the last item"
        );

        assert_eq!(step_focus(2, 2, 3, 1, false), (1, 1));
        assert_eq!(step_focus(1, 1, 3, 1, false), (0, 0));
        assert_eq!(
            step_focus(0, 0, 3, 1, false),
            (0, 0),
            "no-op at the first item"
        );
    }

    #[test]
    fn step_focus_empty_list_resets_pair() {
        // An empty list (e.g. a filter with no matches) resets the pair so a
        // stale highlight cannot linger.
        assert_eq!(step_focus(3, 5, 0, 10, true), (0, 0));
        assert_eq!(step_focus(3, 5, 0, 10, false), (0, 0));
    }

    #[test]
    fn step_focus_stale_scroll_is_clamped_symmetrically() {
        // A stale scroll hint (past the new max_scroll after a filter
        // narrowing) is clamped to the valid range before stepping, so ↑ and
        // ↓ treat the window identically: both clamp in one step instead of
        // the old ↑ behaviour of decrementing the stale hint one press at a
        // time.  len=11 in a 10-row window → max_scroll=1; scroll=10 is stale.
        assert_eq!(
            step_focus(10, 10, 11, 10, false),
            (9, 1),
            "↑ clamps scroll and walks the highlight up"
        );
        assert_eq!(
            step_focus(5, 10, 11, 10, true),
            (6, 1),
            "↓ clamps scroll and walks the highlight down"
        );
    }

    // ── filtered_len (count without building the list) ──────────────────

    #[test]
    fn filtered_len_matches_filtered_count() {
        // The count-only helper must agree with building the full filtered
        // list (they share `filter_matches`), so navigation and rendering
        // can never disagree about the list length.
        let mut sel = ModelSelectorState::new();
        sel.apply_models(
            ["gpt-4o", "gpt-4o-mini", "claude-3"]
                .map(choreo_proto::ModelInfo::plain)
                .to_vec(),
            None,
            false,
        );
        assert_eq!(sel.filtered_len(), 3, "empty filter returns every model");
        sel.filter.text = "gpt".to_string();
        assert_eq!(sel.filtered_len(), 2, "case-insensitive substring");
        assert_eq!(sel.filtered().len(), 2, "agrees with the built list");
        sel.filter.text = "nope".to_string();
        assert_eq!(sel.filtered_len(), 0);
    }
}
