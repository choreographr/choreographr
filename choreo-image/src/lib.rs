//! Shared image decode helpers, used by both the daemon (vision normalization
//! for `read_image`, and `display_image`) and the TUI (client display decode).
//!
//! Centralizes the decode paths that would otherwise be duplicated between
//! `choreo-daemon` and `choreo-tui`, so a security guard or a colour fix lands in
//! the model path and the UI path together and they cannot drift:
//!
//! - [`raster`] — the `image`-crate decode with EXIF orientation baked in
//!   ([`decode_raster_oriented`]) and the header-only size probe
//!   ([`probe_dimensions`]), both under one decompression-bomb guard whose
//!   authoritative bound is a total-pixel budget checked against the declared
//!   size before any buffer is allocated.
//! - [`heic`] — the pure-Rust `heifer` HEIC/HEIF decode ([`decode_heic`]) and its
//!   header-only geometry probe ([`probe_heic_dimensions`]). `heifer` enforces
//!   the same pixel budget itself, against the picture it is about to allocate,
//!   and rejects picture-less or unsupported files rather than guessing.
//! - [`svg`] — the `usvg`/`resvg` parse-and-rasterize path ([`svg::parse`],
//!   [`svg::render`]), where the parse policy (a nesting depth cap bound to
//!   untrusted input, one process-wide font database) and the straight-alpha
//!   output convention live so neither can drift between the model path and the
//!   UI path.
//! - `colour` — the ICC→sRGB conversion for the raster path.
//!
//! # Colour management
//!
//! Every path hands back sRGB, so a `Display-P3` or `AdobeRGB` source reaches the
//! model and the terminal as sRGB values instead of having its numbers
//! reinterpreted as sRGB (which reads visibly desaturated). The two paths own
//! their conversion separately because the decoders expose it differently:
//!
//! - **Raster** buffers are decoded unmanaged by the `image` crate, so this crate
//!   converts the embedded ICC profile with the pure-Rust `moxcms` CMM (see
//!   `colour`).
//! - **HEIC** colour is converted inside `heifer`'s own decode: it applies the
//!   embedded ICC profile when the container carries one, and otherwise the
//!   container's `nclx` description (or the HEVC VUI's primaries and transfer
//!   characteristics). This crate must therefore *not* re-apply a profile to a
//!   HEIC buffer — the pixels are already sRGB on arrival.
//!
//! Both are **fail-soft**: a colour tag never fails an otherwise good image, and
//! a source that carries no colour information is left exactly as decoded
//! (assumed sRGB).
//!
//! # Budgets
//!
//! [`MAX_DECODE_PIXELS`] is the memory guard for both paths, and it is a *total*
//! budget rather than a per-side one, so a tall screenshot or a wide panorama is
//! admitted while a decompression bomb of any shape is rejected.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

use tracing::warn;

mod colour;
pub mod heic;
pub mod raster;
pub mod svg;

pub use heic::{decode_heic, probe_heic_dimensions};
pub use raster::{decode_raster_oriented, probe_dimensions};

/// Primary decompression-bomb guard: the total decoded-pixel budget (px).
///
/// This is the bound that matters — the worst-case allocation is this many
/// pixels at four bytes each, so it is the ceiling behind
/// [`MAX_DECODE_ALLOC`]. It is a *total* budget, not a per-side one, so a tall
/// screenshot or wide panorama (a large single side but modest area) is admitted
/// while a source of any shape with more pixels is rejected before the decoder
/// allocates. The raster path checks it against the image's declared size in
/// [`decode_raster_oriented`]; the HEIC path passes it to `heifer` as its
/// `max_pixels` limit and checks it against the container's declared geometry in
/// [`probe_heic_dimensions`] and in the decode's own header pre-check.
pub const MAX_DECODE_PIXELS: u64 = 8192 * 8192;

/// Cap on total raster decoder allocation (bytes) — derived from (and consistent
/// with) [`MAX_DECODE_PIXELS`]: an RGBA8 image is four bytes per pixel, so the
/// byte budget is the pixel budget times four. Passed to the `image` crate as
/// [`image::Limits::max_alloc`] as defense-in-depth for decoders that honor it;
/// the authoritative raster bound is the declared-pixel check in
/// [`decode_raster_oriented`], which holds even for a codec that ignores it.
///
/// Colour management then needs a second buffer of the decoded sample size, so
/// the raster peak for an image at this ceiling is about twice the budget, not
/// the budget itself — and only for a source that carries a non-sRGB profile.
pub const MAX_DECODE_ALLOC: u64 = MAX_DECODE_PIXELS * 4;

/// Generous sanity cap on a single declared raster side (px).
///
/// The pixel budget ([`MAX_DECODE_PIXELS`]) is the memory guard; this only
/// rejects a nonsensical single dimension (e.g. a corrupt header declaring a
/// side near `u32::MAX`) before a decoder sees it. It is set far above any real
/// image side so it never rejects a legitimate tall screenshot — aspect ratio
/// must not be what a bounded-area guard keys on.
pub const MAX_SOURCE_DIMENSION: u32 = 1 << 16;

/// Why an image could not be decoded or measured.
///
/// The variants separate the two failures a caller may need to treat
/// differently: a well-formed image that is over the decode budget (a policy
/// rejection) from bytes that are not a decodable image at all.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The bytes are not a format the `image` crate recognizes.
    #[error("the raster format is not recognized")]
    UnrecognizedFormat,
    /// The image declares more pixels than the decode budget allows.
    #[error("image is {width}x{height} ({pixels} pixels), over the {budget} pixel decode budget")]
    OverBudget {
        /// The declared width, in pixels.
        width: u32,
        /// The declared height, in pixels.
        height: u32,
        /// The declared pixel count (`width * height`).
        pixels: u64,
        /// The budget it exceeded — normally [`MAX_DECODE_PIXELS`].
        budget: u64,
    },
    /// The `image` crate could not open a decoder or decode the pixels.
    #[error("failed to decode raster image: {0}")]
    Raster(#[source] image::ImageError),
    /// The HEIC/HEIF decoder rejected the file.
    #[error("failed to decode heic: {0}")]
    Heic(#[source] heifer::Error),
    /// The HEIC container does not declare a geometry that can be trusted.
    #[error("the heic container cannot be measured: {0}")]
    HeicGeometry(String),
    /// A decoder returned a buffer that does not match its declared size.
    #[error("the decoded buffer does not match the declared {width}x{height} size")]
    BufferSize {
        /// The declared width, in pixels.
        width: u32,
        /// The declared height, in pixels.
        height: u32,
    },
}

/// Reject a declared geometry whose pixel count exceeds [`MAX_DECODE_PIXELS`].
///
/// Shared by the raster header guard ([`raster::probe_dimensions`] /
/// [`decode_raster_oriented`]) and the HEIC declared-geometry checks
/// ([`probe_heic_dimensions`] and the decode's header pre-check) so every path
/// reports the *same* [`DecodeError::OverBudget`] for an over-budget source — a
/// caller that treats "too many pixels" as a policy rejection matches one variant
/// regardless of the incoming format, rather than a structured error on rasters
/// and a string on HEIC.
///
/// The bound is on the total area, not a per-side dimension (see
/// [`MAX_DECODE_PIXELS`]), so a tall screenshot or wide panorama with few enough
/// pixels is admitted while a bomb of any shape is rejected. `u64` arithmetic
/// cannot overflow a pair of `u32` sides.
pub(crate) fn check_pixel_budget(width: u32, height: u32) -> Result<(), DecodeError> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        warn!(
            width,
            height,
            pixels,
            budget = MAX_DECODE_PIXELS,
            "rejecting image: declared geometry is over the decode budget"
        );
        return Err(DecodeError::OverBudget {
            width,
            height,
            pixels,
            budget: MAX_DECODE_PIXELS,
        });
    }
    Ok(())
}
