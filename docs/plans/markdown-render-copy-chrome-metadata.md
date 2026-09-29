# Plan: First-class copy-chrome metadata in the markdown renderer

**Status:** implemented. Landed as the series `90e0707` (buffer + plumbing) →
`3db637c` (consumers subtract) → `6497c60` (producers emit chrome; the
string/colour detection — `copyable_columns`, `CODE_PANEL_PAD`, `is_panel_*` —
deleted) → `54cd17a` (code blocks render as a table-style bordered box).
Deferred follow-up to the `choreo-tui` markdown-renderer module split.
**Date:** 2026-09-29

> **Note on the code anchors below.** The plan was written before the code-block
> "panel" work landed, so it names `leading_quote_prefix` — by the time it was
> implemented that detector was `copyable_columns`, and it (plus the
> `CODE_BG`/`CODE_PANEL_PAD` sentinel machinery) has now been deleted entirely.
> The appendix code anchors are therefore historical.
**Target:** `choreo-tui` only. No daemon, wire (`choreo-proto`), or schema
change; no persisted state changes.
**Touches:** `choreo-tui/src/markdown_render/` (`mod.rs`, `block.rs`),
`choreo-tui/src/selection.rs`, `choreo-tui/src/state/mod.rs`,
`choreo-tui/src/render/mod.rs` (and their test modules).

> **TL;DR.** The renderer decides which columns of a rendered row are
> *selectable content* versus *non-selectable chrome* by re-scanning the finished
> `Line`'s spans for a magic `(content string, foreground colour)` pair
> (`leading_quote_prefix` recognises the block-quote bar). Two problems follow:
> the coupling is fragile (any span that happens to match is treated as chrome),
> and it can only recognise chrome that is the **leading run** of a line — so a
> block quote nested directly inside a list item copies its `│ ` bar. This plan
> replaces the re-scan with **first-class per-line chrome metadata emitted by the
> renderer** and has the selection/copy machinery subtract it. That removes the
> string/colour contract and fixes the nested-quote case generally, while leaving
> the existing `content_ranges` contract intact.

---

## Table of contents

1. [Motivation & current design](#1-motivation--current-design)
2. [Goal & non-goals](#2-goal--non-goals)
3. [Design](#3-design)
4. [Decisions log](#4-decisions-log)
5. [Work breakdown](#5-work-breakdown)
6. [Phased execution plan](#6-phased-execution-plan)
7. [Testing strategy](#7-testing-strategy)
8. [Risks & mitigations](#8-risks--mitigations)
9. [Out of scope / future work](#9-out-of-scope--future-work)
10. [Open questions](#10-open-questions)
11. [Verification / definition of done](#11-verification--definition-of-done)
12. [Appendix — current code anchors](#12-appendix--current-code-anchors)

---

## 1. Motivation & current design

### 1.1 Where copy ranges come from today

`render_turn_lines` returns a `RenderedTurnLines` whose `content_ranges` vector
is aligned with `lines`: each entry is `Option<(lo, hi)>` — the display-column
interval a selection may highlight and copy. `None` means "pure chrome" (box
separators, padding), and an empty `(lo, lo)` range means "a blank *content*
row" (so a spacer between markdown blocks survives a copy as a blank line).

`choreo-tui/src/selection.rs` maps every visual row to its semantic line and that
line's `(lo, hi)` via `content_range_for_row`, then:
- `apply_selection_to_lines` → `style_line_selection` draws the highlight for the
  clamped interval; and
- `extract_selection_text` → `text_and_join_for_content_line` →
  `slice_line_columns` copies the clamped interval's text,
- gluing rows with the per-line `LineJoin` (`Break`/`Space`/`Join`).

Two functions in `choreo-tui/src/markdown_render/mod.rs` compute
`content_ranges`:
- `add_margin_lines` (user/assistant/tool-result message rows, which carry the
  `"  ┃  "` gutter), and
- the tool-body loop inside `render_turn_lines` (unboxed rows, no gutter).

Both call `leading_quote_prefix(line)` to measure a run of leading block-quote
bar spans and push `content_ranges = Some((base + prefix, base + text_width))` so
the bar is excluded.

### 1.2 Defect 1 — the `(content, colour)` coupling

`leading_quote_prefix` recognises a bar span by exact string **and** foreground
colour:

```rust
if span.content.as_ref() == QUOTE_BAR && span.style.fg == Some(QUOTE_BAR_COLOR) {
    width += QUOTE_BAR_WIDTH;
} else {
    break;
}
```

This is a contract between the *producer* (the `BlockQuote` arm in `block.rs`,
which inserts `Span::styled(QUOTE_BAR, Style::default().fg(QUOTE_BAR_COLOR))`)
and a *consumer* that re-derives the producer's intent from the rendered bytes.
It is verified collision-free **today** (the diff renderer's own `│` gutter uses
`"│"` with no trailing space; the only `"│ "`/`DarkGray` span in the renderer is
`QUOTE_BAR`), but nothing enforces that invariant going forward: a future span
that renders as `"│ "` in `DarkGray` — an ANSI-coloured tool stream, a new gutter
glyph — would be silently swallowed from copies. The coupling is documented only
in a doc comment.

### 1.3 Defect 2 — leading-run-only (nested quote in a list)

`leading_quote_prefix` stops at the first non-bar span. When a block quote is
nested directly inside a list item, `render_markdown_block`'s `List` arm prepends
the marker span **before** delegating to the item's inner blocks, so the row is
`[indent + marker][bar][text]`. The first span is not a bar, so the function
returns `0` and the whole row — including the `│ ` bar — is copied. The current
code documents this as a "rare, graceful degradation". It is not rare in
LLM output (lists of quotes are common), and the degradation is visible.

### 1.4 Why the current representation cannot fix §1.3

An entry in `content_ranges` is a **single contiguous** `(lo, hi)`. The nested
case wants to *keep* the list marker (`lo … bar_lo`) **and** the quote text
(`bar_hi … end`) while *dropping* the interior bar. No single `(lo, hi)` expresses
that: excluding the bar by moving `lo` past it also drops the marker. A
representation that can name more than one selectable interval (or name the
chrome to subtract) is required.

---

## 2. Goal & non-goals

**Goal.** The renderer is the single source of truth for which columns of a row
are chrome. It emits that as first-class, typed, per-line metadata; the
selection/copy machinery consumes it without inspecting span text or colour.

**In particular:**
- delete `leading_quote_prefix` and the `(content, colour)` match entirely;
- fix the nested-in-list quote so its bar is never copied or highlighted;
- keep the existing external contract (`content_ranges` semantics, `LineJoin`,
  `RenderedTurnLines` shape) as close to unchanged as practical, because several
  call sites and cached caches key on it.

**Non-goals.**
- No change to *what* is chrome vs. content beyond the nested-quote fix (list
  markers stay copyable, as today).
- No change to wrapping, height, or hit-testing (`compute_visual_offsets`,
  `style_line_selection`'s clipping model) other than threading the extra data.
- No wire/protocol/persistence change — all of this is client-local rendering.

---

## 3. Design

### 3.1 New type: per-line chrome intervals

Add a small, cheap, copyable type next to `LineJoin` in `mod.rs`:

```rust
/// Display-column intervals of a rendered line that are **non-selectable
/// chrome** (the value is relative to the line buffer its producer built).
/// Empty for the overwhelming majority of lines; a `SmallVec` keeps the common
/// empty case allocation-free.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct LineChrome(SmallVec<[(u16, u16); 1]>);

impl LineChrome {
    pub(crate) fn push(&mut self, lo: usize, hi: usize) { /* checked into u16 */ }
    pub(crate) fn is_empty(&self) -> bool { self.0.is_empty() }
    pub(crate) fn intervals(&self) -> &[(u16, u16)] { &self.0 }
}
```

`u16` is ample (display columns per row are viewport-bounded) and keeps the type
`Copy`-ish/small; saturate or `debug_assert` on overflow exactly as the existing
width math does.

### 3.2 Producers emit chrome (the metadata)

The renderer builds lines in two layers; both must carry chrome:

1. **Block layer** (`block.rs`). `render_markdown_blocks` /
   `render_markdown_block` already push `lines` and `joins` in lockstep. Thread a
   third parallel buffer, `chrome: &mut Vec<LineChrome>`, beside them (or return a
   single `Vec<RenderedRow>` bundling `{ line, join, chrome }`; the parallel-vec
   form is the smaller diff and matches the existing `lines`/`joins` pattern). The
   `BlockQuote` arm records the bar's interval (`0..QUOTE_BAR_WIDTH`) on each row
   it prepends the bar to. Nested quotes naturally record two intervals. The
   `List` arm, which prepends the marker span **in front of** the already-built
   item rows, must **shift** every inner chrome interval right by the marker
   width when it clones the inner line's spans (the marker itself is content, not
   chrome).

2. **Assembly layer** (`mod.rs`). `add_margin_lines` prepends the
   `"  ┃  "` gutter (a 5-column prefix) and `render_turn_lines`'s tool-body loop
   prepends nothing. Both translate each inner chrome interval by their own
   prefix offset, then compute `content_ranges` as today **minus** the chrome
   (see §3.3). The margin gutter itself stays expressed by the `base` offset (it
   is leading chrome outside any content range) — no new machinery needed there.

In short: **the only new producer work is the `BlockQuote` arm recording its bar,
and the `List` arm shifting inner intervals.** Everything else just carries the
buffer through.

### 3.3 Consumers subtract chrome

`RenderedTurnLines` gains one aligned field:

```rust
pub(crate) struct RenderedTurnLines {
    pub lines: Vec<Line<'static>>,
    pub joins: Vec<LineJoin>,
    pub content_ranges: Vec<Option<(usize, usize)>>,   // unchanged
    pub chrome_ranges: Vec<LineChrome>,                // new, aligned with lines
    pub reasoning_header_idx: Option<usize>,
    pub tool_result_header_idxs: Vec<usize>,
}
```

`RenderedTurn` in `state/mod.rs` mirrors it (`chrome_ranges: Arc<[LineChrome]>`),
populated in `cached_or_compute_lines` and the test/fixture constructors.

This keeps `content_ranges: Option<(usize, usize)>` exactly as-is. The **chrome is
a set of exclusions within** that base range. Both consumers then operate on the
**selectable sub-intervals** `content_range − chrome`:

- Add one helper in `selection.rs`, e.g.
  `selectable_intervals(range: (usize, usize), chrome: &LineChrome) -> SmallVec<[(usize,usize);2]>`,
  which returns the base range with each chrome interval removed (merging
  overlaps, clamping out-of-range). This is the **only** place the subtraction
  logic lives.
- `content_range_for_row` keeps returning `(line_idx, (lo, hi))` for the *base*
  range (so the row→line mapping and the `None`/blank semantics are untouched),
  and additionally returns/exposes the line's chrome so the caller can split.
- `style_line_selection` generalises to a list of intervals (it already splits
  spans per column; iterate the union of selected sub-intervals instead of one
  `[col_lo, col_hi)`). The highlight is the union, so styling each sub-interval
  in one pass is straightforward.
- `text_and_join_for_content_line` / `slice_line_columns` concatenate the
  selected slices of each sub-interval (order preserved), so a copy of
  `- │ quote` yields `- quote`.

### 3.4 Worked example

Markdown: `- > hello`. After rendering (tool-body/unboxed path, width W):

| span              | columns   | role                        |
|-------------------|-----------|-----------------------------|
| `"- "` (marker)   | 0..2      | content (list marker)       |
| `"│ "` (bar)      | 2..4      | **chrome**                  |
| `"hello"`         | 4..9      | content                     |

- `content_ranges[row] = Some((0, 9))` (base, unchanged kind).
- `chrome_ranges[row] = [(2, 4)]`.
- Selectable = `[(0,2), (4,9)]` → the copy is `- hello`; the highlight skips the
  bar; the bar glyph is never in the clipboard.

Top-level `> hello` (bar is leading):

- `content_ranges[row] = Some((0, 7))`, `chrome_ranges[row] = [(0, 2)]`,
  selectable `[(2, 7)]` → copy `hello`. (Equivalent to today's
  `content_ranges = (2, 7)`; the representation moved the exclusion from the base
  into the chrome list — see §4 A.)

---

## 4. Decisions log

**A. Base range + chrome exclusions, not a selectable-interval *list* as the
stored type.**
`content_ranges` stays `Option<(usize, usize)>`; chrome is a separate aligned
array. Rationale: `content_ranges` is consumed by `state/mod.rs`,
`render/mod.rs`, `selection.rs`, and many test fixtures, and is **cached**
(`RenderedTurn.content_ranges: Arc<[...]>`); changing its element type ripples
through all of them. Adding one aligned `ChromeRanges` array confines the change
to the producer side plus a single subtraction helper. *Rejected: store
`Option<SmallVec<[(usize,usize);2]>>` directly (cleanest for consumers, larger
type churn through the cache and fixtures, and loses the simple
`None`/blank semantics distinction unless re-encoded).*

**B. Chrome is produced by the block renderer, not inferred at assembly.**
The `BlockQuote` arm is the only place that *knows* a bar is chrome. Inferring it
later is exactly the string/colour match we are deleting. *Rejected: keep a
detector but key it on a distinct wrapper span type — `ratatui::Span` has no
spare channel, and a wrapper would fight every `Line`/`Span` construction site.*

**C. Thread the chrome buffer as a third parallel `Vec`, not a bundle struct.**
`lines`/`joins` are already pushed in lockstep across ~15 sites in `block.rs`;
adding `chrome.push(...)` next to each is mechanical. *Rejected: return
`Vec<RenderedRow>` bundling `{line, join, chrome}` — cleaner in the abstract but
touches every producer signature and every zip; larger, riskier diff for the same
result. Revisit if a fourth aligned array ever appears.*

**D. Keep list markers copyable.**
The nested case keeps `- ` and drops only the bar. Changing marker policy is out
of scope and would alter many existing copy tests.

**E. `LineChrome` uses `u16` columns.**
Rows are viewport-width-bounded; `u16` halves the type's footprint and keeps it
`Copy`. Saturating conversion with a `debug_assert` mirrors the existing width
math.

---

## 5. Work breakdown

- **`markdown_render/mod.rs`**: add `LineChrome`; add the `chrome` parameter to
  `render_markdown_blocks`/`render_markdown_block`/`add_margin_lines` and the
  tool-body loop; add `RenderedTurnLines.chrome_ranges`; delete
  `leading_quote_prefix`; translate inner chrome by each layer's prefix offset;
  compute `content_ranges` unchanged and fill `chrome_ranges`.
- **`markdown_render/block.rs`**: the `BlockQuote` arm records the bar interval;
  the `List` arm shifts inner chrome by the marker width when it prepends the
  marker span; pass `chrome` through recursion.
- **`selection.rs`**: add `selectable_intervals`; thread `chrome_ranges` into
  `content_range_for_row` call sites (`apply_selection_to_lines`,
  `text_and_join_for_content_line`, `extract_selection_text`); generalise
  `style_line_selection` and the copy path to the interval list.
- **`state/mod.rs`**: add `chrome_ranges` to `RenderedTurn`, populate in
  `cached_or_compute_lines` and the constructors, extend the alignment `debug_assert`s.
- **`render/mod.rs`**: pass `chrome_ranges` into `apply_selection_to_lines`
  alongside `content_ranges`; update the test fixtures
  (`RenderedTurnLines { … }`) that construct the struct literally.
- **`ARCHITECTURE.md`**: update the `markdown_render/` row to describe
  renderer-emitted chrome and note that block-quote-in-list rows are now
  copy-clean.

## 6. Phased execution plan

Each phase is independently shippable, tested, and **committed** (per
`AGENTS.md`: one subsession per task, in series, full `just pre-commit` gate,
commit before returning).

- **Phase 0 — Type + plumbing (no behaviour change).** Add `LineChrome`,
  `chrome_ranges`, and the third parallel buffer; every producer pushes an empty
  chrome value. Consumers ignore it. Green gate; no test deltas.
- **Phase 1 — Emit the bar.** `BlockQuote` records the bar; `add_margin_lines`
  and the tool-body loop translate and fill `chrome_ranges`. Switch top-level
  quote rows to base+chrome (update the two existing quote copy-range tests).
  Selection still subtracts nothing — verify no regression, then:
- **Phase 2 — Consume chrome.** Add `selectable_intervals`; thread chrome into
  `selection.rs`; generalise highlight + copy. Delete `leading_quote_prefix`.
  Add the nested-in-list quote copy test.
- **Phase 3 — Docs.** `ARCHITECTURE.md` row; mark this plan done.

## 7. Testing strategy

- **Nested quote copy (the fix):** `render_turn_lines` for a turn whose text is
  `- > hello` (and a deeper `- > > hi`) → copy yields `- hello` / `- > hi`
  *without* any `│`; the highlight range for the bar columns is empty.
- **Top-level quote unchanged:** existing
  `markdown_blockquote_bar_is_excluded_from_copy_range` /
  `render_turn_lines_quote_bar_is_not_copyable` /
  `render_turn_lines_tool_markdown_quote_bar_is_not_copyable` still pass (spirit
  preserved under the new representation).
- **No false-positive chrome:** a row whose text merely *contains* `│ ` (default
  or ANSI-coloured) and a ` ```diff ` body's own `│` gutter are copied in full —
  a direct guard against the coupling this plan removes.
- **Chrome-free rows unchanged:** every non-quote markdown, plain-text, table,
  and diff row has an empty `chrome_ranges` and identical `content_ranges`
  (property-style: shuffle the existing markdown_render test corpus and assert
  `chrome_ranges` all-empty except quote rows).
- **Blank-row semantics:** the empty `(lo, lo)` blank-content row still copies a
  blank line and still has empty chrome.
- **Alignment invariants:** the `debug_assert_eq!`s in `cached_or_compute_lines`
  extended to `chrome_ranges.len() == lines.len()`; the render-cache key is
  unaffected (chrome is a pure function of the same inputs).

## 8. Risks & mitigations

- **Wide mechanical diff** (a third buffer across the block renderer). Mitigation:
  Phase 0 lands the plumbing with empty chrome and no behaviour change, so any
  regression is caught immediately and bisectable.
- **Chrome-shift arithmetic in the `List`/margin layers** (off-by-marker-width
  bugs). Mitigation: the shift is applied in exactly two places (the list arm and
  the margin/tool-body offsets); cover both with the nested-quote tests.
- **Cache-consistency** (`RenderedTurn` is cached and keyed on width/turn id).
  Mitigation: `chrome_ranges` derives deterministically from the same inputs as
  `content_ranges`; the alignment asserts make a mismatch a debug-build failure.
- **Fixture churn** (struct literals in `state`/`render` tests). Mitigation: give
  `RenderedTurnLines`/`RenderedTurn` a `Default`-ish constructor helper, or add
  the field with `#[serde(...)]`-free plain struct updates — bounded, mechanical.

## 9. Out of scope / future work

- **Other chrome sources** (list markers, code-fence glyphs, table borders) could
  adopt `LineChrome` later, but none is needed for the goal.
- **Partial-row chrome for hit-testing** (e.g. clicking a bar should not start a
  selection inside it). The selection already clamps to selectable intervals after
  this change, so this falls out for free; no extra work planned.
- **A styled-span "role" channel at the ratatui layer** (would remove the parallel
  array) — out of our control (`ratatui::text::Span` has no metadata slot).

## 10. Open questions

- Whether to bundle `{line, join, chrome}` now that a third aligned array exists
  (Decision §4 C chose parallel vectors; revisit if a fourth appears).
- Whether the tool-body loop and `add_margin_lines` should share one
  "translate chrome by prefix" helper (likely yes; it is the same arithmetic).

## 11. Verification / definition of done

- `just pre-commit` green in one pass (clippy strict + all tests + fmt).
- A nested-in-list block quote copies without its `│ ` bar (new test).
- No occurrence of `leading_quote_prefix` / `QUOTE_BAR_COLOR`-based detection
  remains in the renderer.
- A row whose text contains `│ ` (including a ` ```diff ` gutter) is copied
  verbatim.
- `ARCHITECTURE.md`'s `markdown_render/` row describes renderer-emitted chrome.

---

## 12. Appendix — current code anchors

- `choreo-tui/src/markdown_render/mod.rs`
  - `QUOTE_BAR` / `QUOTE_BAR_COLOR` / `QUOTE_BAR_WIDTH` (constants).
  - `leading_quote_prefix` — the detector this plan deletes.
  - `add_margin_lines` — fills `content_ranges` for boxed message rows.
  - `render_turn_lines` — the tool-body loop fills `content_ranges`; returns
    `RenderedTurnLines`.
  - `LineJoin`, `RenderedTurnLines`.
- `choreo-tui/src/markdown_render/block.rs`
  - `render_markdown_block` `BlockQuote` arm — inserts the `QUOTE_BAR` span
    (the chrome producer).
  - `render_markdown_block` `List` arm — prepends the marker span before the
    inner rows (the shift site).
- `choreo-tui/src/selection.rs`
  - `content_range_for_row` — row→line→column mapping shared by highlight and
    copy.
  - `apply_selection_to_lines` / `style_line_selection` — highlight.
  - `extract_selection_text` / `text_and_join_for_content_line` /
    `slice_line_columns` — copy.
- `choreo-tui/src/state/mod.rs`
  - `RenderedTurn` (`content_ranges: Arc<[Option<(usize, usize)>]>`) and
    `cached_or_compute_lines` (alignment asserts).
- `choreo-tui/src/render/mod.rs`
  - passes `content_ranges` into `apply_selection_to_lines`.
