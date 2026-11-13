//! The SVG parse and rasterize path, shared by the daemon (vision
//! normalization for `read_image`, and `display_image`) and the TUI (client
//! display decode).
//!
//! SVG is the one image format Choreographr accepts as *code* rather than
//! pixels: `usvg` resolves `<use>` and `xlink:href` chains, CSS, gradients,
//! patterns, clip paths, masks and filters before a single pixel exists. That
//! resolution walks references an author controls, so the parser is the part of
//! the image path that needs a bound — which is why the policy lives here, once,
//! instead of at each call site where the model path and the UI path could
//! drift apart:
//!
//! - [`parse`] applies [`MAX_SVG_DEPTH`] and one process-wide
//!   [`usvg::Options`], so every consumer admits the same documents.
//! - [`render`] hands back **straight** (non-premultiplied) RGBA, the alpha
//!   convention `image` uses, so a translucent fill reaches the model and the
//!   terminal at the colour the SVG specifies.
//!
//! The two halves are deliberately split: parsing is a policy decision and
//! belongs here, while *how much* of the tree to draw is the caller's, because
//! the daemon renders an optional crop region at native resolution and the TUI
//! renders a whole frame scaled to fit its cell — different targets, same
//! policy.

use std::sync::{Arc, LazyLock};

use image::RgbaImage;
/// The renderer's two type-carrying crates, re-exported so a caller can name the
/// [`usvg::Tree`] this module hands back and build the [`tiny_skia::Transform`]
/// it renders with, without taking its own dependency on `resvg` (which would
/// let two callers resolve different renderer versions).
pub use resvg::{tiny_skia, usvg};

/// Maximum element nesting depth accepted when parsing an SVG.
///
/// `usvg`'s own default is 1024, a backstop chosen to stop "billion laughs"
/// style documents rather than to fit real art; every SVG we have seen nests in
/// the tens. Choreographr parses SVGs it does not control — markup a model wrote
/// into a `display_image` argument, bytes fetched from a URL, a file the user
/// pointed at — so this is tightened to a bound no legitimate illustration
/// reaches while keeping the parser well away from stack exhaustion. A document
/// deeper than this is refused at parse time — [`usvg::Error::ParsingFailed`]
/// carrying the XML parser's node/depth limit error — rather than truncated, so
/// a caller never renders a silently mangled tree.
pub const MAX_SVG_DEPTH: u32 = 256;

/// The one [`usvg::Options`] every parse in the process shares.
///
/// Built lazily because loading the system font database scans the OS font
/// directories, which costs far more than parsing a document: `Options` holds
/// its `fontdb` behind an `Arc`, so one scan serves every render. Fonts are
/// load-bearing rather than cosmetic — they decide the metrics `usvg` reports for
/// `<text>`, which feeds `Tree::size()` and therefore the geometry a caller
/// renders into. A failure to load is non-fatal: glyphs that cannot be resolved
/// are skipped.
///
/// The catalogue is fixed for the life of the process, so a font installed while
/// the daemon or TUI runs is picked up on the next start. That is the right trade
/// for a long-lived process: nothing here is content-dependent, and a re-scan per
/// image would put a filesystem walk in the vision and display paths.
static OPTIONS: LazyLock<usvg::Options<'static>> = LazyLock::new(|| {
    // `max_depth` is set in the initializer rather than by reassignment so the
    // policy and the rest of the defaults are read in one place.
    let mut options = usvg::Options {
        max_depth: MAX_SVG_DEPTH,
        ..usvg::Options::default()
    };
    Arc::make_mut(&mut options.fontdb).load_system_fonts();
    options
});

/// Why an SVG could not be rasterized.
///
/// Both variants are allocation refusals, kept apart because they mean different
/// things to a caller: a target the rasterizer cannot allocate at all, versus a
/// rasterizer that returned a buffer of the wrong length (which would be a bug in
/// `resvg`/`tiny-skia` rather than in the caller).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// The requested raster target is not a size a pixmap can be allocated at
    /// (zero on either side, or past `tiny-skia`'s per-side limit).
    #[error("svg raster target {width}x{height} is not a valid pixmap size")]
    TargetSize {
        /// The requested width, in pixels.
        width: u32,
        /// The requested height, in pixels.
        height: u32,
    },
    /// The rasterizer returned a buffer that does not match the requested size.
    #[error("the rasterized buffer does not match the {width}x{height} target")]
    BufferSize {
        /// The requested width, in pixels.
        width: u32,
        /// The requested height, in pixels.
        height: u32,
    },
}

/// Parse SVG bytes into a resolved [`usvg::Tree`].
///
/// Applies the shared [`Options`](usvg::Options) — the [`MAX_SVG_DEPTH`] nesting cap and the
/// process-wide system font database — so every caller admits the same documents
/// and sizes `<text>` the same way. The returned tree carries absolute transforms
/// and resolved references: it is what [`render`] consumes, and `Tree::size()`
/// reports the viewport in the user units a caller's crop or scale math works in.
///
/// # Errors
///
/// Returns [`usvg::Error`] when the bytes are not UTF-8
/// ([`usvg::Error::NotAnUtf8Str`]), are not SVG or are malformed
/// ([`usvg::Error::ParsingFailed`]), carry no usable size
/// ([`usvg::Error::InvalidSize`]), exceed the parser's element limit
/// ([`usvg::Error::ElementsLimitReached`]), or nest deeper than [`MAX_SVG_DEPTH`]
/// ([`usvg::Error::ParsingFailed`], carrying the XML parser's node/depth limit).
pub fn parse(data: &[u8]) -> Result<usvg::Tree, usvg::Error> {
    usvg::Tree::from_data(data, &OPTIONS)
}

/// Rasterize a parsed SVG into an RGBA image of exactly `width`×`height`.
///
/// `transform` maps user units onto the pixmap, so a caller controls both scale
/// and the region drawn: an identity transform draws the whole viewport at its
/// intrinsic size, and a scaled/translated one draws a sub-region at full vector
/// detail (cropping by *rendering only the region*, rather than downscaling the
/// whole tree and cutting afterwards, is what preserves the detail a crop is
/// meant to recover). Content outside the pixmap is clipped.
///
/// The pixels are handed back **straight** (non-premultiplied) RGBA. `tiny-skia`
/// stores premultiplied alpha internally, so the conversion is done here with
/// `Pixmap::take_demultiplied` — a translucent fill read out premultiplied and
/// reinterpreted as straight would arrive darker than the document specifies,
/// which is invisible on opaque art and wrong on everything else.
///
/// # Errors
///
/// Returns [`RenderError::TargetSize`] when `width`/`height` cannot be allocated
/// as a pixmap, and [`RenderError::BufferSize`] if the rasterizer returns a buffer
/// of the wrong length.
pub fn render(
    tree: &usvg::Tree,
    transform: tiny_skia::Transform,
    width: u32,
    height: u32,
) -> Result<RgbaImage, RenderError> {
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or(RenderError::TargetSize { width, height })?;
    resvg::render(tree, transform, &mut pixmap.as_mut());
    RgbaImage::from_raw(width, height, pixmap.take_demultiplied())
        .ok_or(RenderError::BufferSize { width, height })
}

#[cfg(test)]
mod tests;
