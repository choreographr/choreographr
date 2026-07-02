use image::{DynamicImage, RgbaImage, load_from_memory};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use resvg::{tiny_skia, usvg};
use std::io;
pub use tai_client_core::{
    ClientError, ImageAssembler, MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline,
    ShellCommand, StreamingText, parse_input_line,
};
use tai_proto::ImageMetadata;
use tokio::sync::mpsc;

pub fn channel_closed<T>(_: mpsc::error::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "connection writer closed")
}

pub struct RenderedImage {
    pub metadata: ImageMetadata,
    pub protocol: StatefulProtocol,
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub fn build_rendered_image(
    picker: &Picker,
    metadata: ImageMetadata,
    data: Vec<u8>,
) -> io::Result<RenderedImage> {
    let image = decode_display_image(&metadata, &data)?;
    let protocol = picker.new_resize_protocol(image);
    Ok(RenderedImage { metadata, protocol })
}

fn decode_display_image(metadata: &ImageMetadata, data: &[u8]) -> io::Result<DynamicImage> {
    if metadata.mime_type == "image/svg+xml" {
        rasterize_svg(data)
    } else {
        load_from_memory(data).map_err(io::Error::other)
    }
}

fn rasterize_svg(data: &[u8]) -> io::Result<DynamicImage> {
    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &options).map_err(io::Error::other)?;
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

#[cfg(test)]
mod lib_tests;
