use image::{DynamicImage, RgbaImage, load_from_memory};
use ratatui::layout::Size;
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use resvg::{tiny_skia, usvg};
use std::io;
use std::sync::Arc;
pub use tai_client_core::{
    ClientError, ImageAssembler, ShellCommand, StreamingText, parse_input_line,
};
pub use tai_markdown::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use tai_proto::ImageMetadata;
pub struct RenderedImage {
    pub metadata: ImageMetadata,
    pub protocol: StatefulProtocol,
    /// Raw SVG bytes, kept permanently so the image can be re-rasterized
    /// at any display resolution (inline, fullscreen, or after terminal
    /// resize).  `None` means this is a PNG/JPEG.
    pub svg_data: Option<Vec<u8>>,
    /// The terminal cell size at which the current protocol was last
    /// rasterized.  `None` means only the initial native-size rasterization
    /// exists.  When `ensure_display_resolution` is called with a matching
    /// cell size, re-rasterization is skipped.
    pub rasterized_cells: Option<Size>,
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub fn build_rendered_image(
    picker: &Picker,
    metadata: ImageMetadata,
    data: Vec<u8>,
) -> io::Result<RenderedImage> {
    let is_svg = metadata.mime_type == "image/svg+xml";
    let svg_data = if is_svg { Some(data.clone()) } else { None };
    let image = decode_display_image(&metadata, &data)?;
    let protocol = picker.new_resize_protocol(image);
    Ok(RenderedImage {
        metadata,
        protocol,
        svg_data,
        rasterized_cells: None,
    })
}

impl RenderedImage {
    /// Ensure the SVG is rasterized at the pixel resolution needed for the
    /// given terminal cell area.  If the image is not an SVG, or if it was
    /// already rasterized at this `cell_size`, this is a no-op.
    ///
    /// When re-rasterization is needed, the SVG vectors are rendered directly
    /// at the target resolution (via `resvg` with a scaled transform), the
    /// old protocol is replaced, and the cell-size is cached so subsequent
    /// calls with the same size are free.
    pub fn ensure_display_resolution(
        &mut self,
        picker: &Picker,
        cell_size: Size,
    ) -> io::Result<()> {
        // Not an SVG — nothing to upgrade.
        let Some(svg_data) = self.svg_data.as_ref() else {
            return Ok(());
        };
        // Cache hit — already rasterized for this cell size.
        if self.rasterized_cells == Some(cell_size) {
            return Ok(());
        }
        let font_size = picker.font_size();
        let target_px_w = cell_size.width as u32 * font_size.width as u32;
        let target_px_h = cell_size.height as u32 * font_size.height as u32;
        let image = rasterize_svg_at_size(svg_data, target_px_w, target_px_h)?;
        self.protocol = picker.new_resize_protocol(image);
        self.rasterized_cells = Some(cell_size);
        Ok(())
    }
}

fn decode_display_image(metadata: &ImageMetadata, data: &[u8]) -> io::Result<DynamicImage> {
    if metadata.mime_type == "image/svg+xml" {
        rasterize_svg(data)
    } else {
        load_from_memory(data).map_err(io::Error::other)
    }
}

/// Parse an SVG into a `usvg::Tree` with system fonts loaded.
fn parse_svg(data: &[u8]) -> io::Result<usvg::Tree> {
    let mut options = usvg::Options::default();
    Arc::make_mut(&mut options.fontdb).load_system_fonts();
    usvg::Tree::from_data(data, &options).map_err(io::Error::other)
}

/// Rasterize an SVG at its native viewport size (used for the initial
/// protocol before re-rasterization).
fn rasterize_svg(data: &[u8]) -> io::Result<DynamicImage> {
    let tree = parse_svg(data)?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "svg dimensions are too large to rasterize",
        )
    })?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    let image =
        RgbaImage::from_raw(size.width(), size.height(), pixmap.take()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "failed to build raster image from svg",
            )
        })?;
    Ok(DynamicImage::ImageRgba8(image))
}

/// Rasterize an SVG at a target pixel resolution, preserving aspect ratio.
/// The resulting image will fit within `target_px_w × target_px_h` pixels.
fn rasterize_svg_at_size(
    data: &[u8],
    target_px_w: u32,
    target_px_h: u32,
) -> io::Result<DynamicImage> {
    let tree = parse_svg(data)?;
    let svg_size = tree.size();

    // Compute a uniform scale that fits the SVG within the target pixel
    // bounds while preserving aspect ratio.  The vector graphics are
    // rendered directly at this resolution so no post-rasterization
    // upscaling by ratatui-image is needed.
    let scale = (target_px_w as f32 / svg_size.width()).min(target_px_h as f32 / svg_size.height());

    let out_w = (svg_size.width() * scale).ceil() as u32;
    let out_h = (svg_size.height() * scale).ceil() as u32;
    let out_w = out_w.max(1);
    let out_h = out_h.max(1);

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "svg dimensions are too large to rasterize",
        )
    })?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let image = RgbaImage::from_raw(out_w, out_h, pixmap.take()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to build raster image from svg",
        )
    })?;
    Ok(DynamicImage::ImageRgba8(image))
}

#[cfg(test)]
mod lib_tests;
