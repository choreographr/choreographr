use choreo_proto::ImageMetadata;
use crossbeam::channel;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use ratatui::layout::Size;
use ratatui_image::{Resize, ResizeEncodeRender, picker::Picker, protocol::StatefulProtocol};
use resvg::{tiny_skia, usvg};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

pub type ImageId = usize;

/// A job submitted to the background image worker.
pub struct ImageJob {
    pub id: ImageId,
    /// Raw image bytes, shared via Arc so no deep copy is needed when
    /// submitting the same image at multiple sizes (inline + fullscreen).
    pub data: Arc<[u8]>,
    pub metadata: ImageMetadata,
    pub cell_size: Size,
    pub resize: Resize,
}

/// The result of a completed encoding job.
///
/// `protocol` is `Some` on success and `None` on failure.  The caller should
/// always clear `pending_job` on receipt so the image can be re-attempted
/// on a subsequent frame.
pub struct ImageResult {
    pub id: ImageId,
    pub protocol: Option<StatefulProtocol>,
    pub cell_size: Size,
}

pub struct ImageWorker {
    pub job_tx: channel::Sender<ImageJob>,
    pub result_rx: channel::Receiver<ImageResult>,
    pub handle: thread::JoinHandle<()>,
}

static NEXT_JOB_ID: AtomicUsize = AtomicUsize::new(1);

pub fn next_job_id() -> ImageId {
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

impl ImageWorker {
    /// Spawn a background thread that processes image encoding jobs.
    ///
    /// The worker owns a clone of `picker` and uses it to create and encode
    /// terminal protocols from raw image bytes.
    pub fn spawn(picker: Picker) -> Self {
        let (job_tx, job_rx) = channel::unbounded::<ImageJob>();
        let (result_tx, result_rx) = channel::unbounded::<ImageResult>();

        let handle = thread::spawn(move || {
            loop {
                match job_rx.recv() {
                    Err(_) => {
                        tracing::debug!("[choreo-tui] image worker shutting down");
                        break;
                    }
                    Ok(job) => process_job(&picker, &result_tx, job),
                }
            }
        });

        ImageWorker {
            job_tx,
            result_rx,
            handle,
        }
    }
}

/// Process a single encoding job: decode the image data (rasterising SVG at
/// the target pixel resolution), create a protocol, and run the expensive
/// terminal-protocol encoding.  Results are sent back on `result_tx`.
///
/// On failure, a result with `protocol: None` is still sent so the caller
/// can clear `pending_job` and allow a retry on the next frame.
fn process_job(picker: &Picker, result_tx: &channel::Sender<ImageResult>, job: ImageJob) {
    tracing::debug!(
        "[choreo-tui] image worker processing job {} ({} {}x{})",
        job.id,
        job.metadata.mime_type,
        job.metadata.width,
        job.metadata.height,
    );

    let font_size = picker.font_size();
    let target_px_w = (job.cell_size.width as u32).saturating_mul(font_size.width as u32);
    let target_px_h = (job.cell_size.height as u32).saturating_mul(font_size.height as u32);

    let image = match decode_image(&job.metadata, &job.data, target_px_w, target_px_h) {
        Ok(img) => img,
        Err(e) => {
            tracing::error!(
                "[choreo-tui] image worker failed to decode image {}: {e}",
                job.id
            );
            // Send a failure result so the UI thread clears pending_job.
            let _ = result_tx.send(ImageResult {
                id: job.id,
                protocol: None,
                cell_size: job.cell_size,
            });
            return;
        }
    };

    let mut protocol = picker.new_resize_protocol(image);

    // Trigger terminal-protocol-specific encoding (the expensive part),
    // which also caches the layout dimensions internally.
    protocol.resize_encode(&job.resize, job.cell_size);

    let result = ImageResult {
        id: job.id,
        protocol: Some(protocol),
        cell_size: job.cell_size,
    };

    if let Err(e) = result_tx.send(result) {
        tracing::error!(
            "[choreo-tui] image worker failed to send result for {}: {e}",
            job.id
        );
    }
}

/// Decode raw image bytes into a [`DynamicImage`].
///
/// SVG data is rasterised directly at `target_px_w × target_px_h`
/// (preserving aspect ratio). HEIC/HEIF goes through the pure-Rust
/// `heif-oxide` decoder (which applies the container's orientation). Raster
/// formats use the `image` crate and have their EXIF orientation baked in, so
/// phone/camera photos render upright.
fn decode_image(
    metadata: &ImageMetadata,
    data: &[u8],
    target_px_w: u32,
    target_px_h: u32,
) -> Result<DynamicImage, String> {
    match metadata.mime_type.as_str() {
        "image/svg+xml" => rasterize_svg_at_size(data, target_px_w, target_px_h),
        "image/heic" | "image/heif" => decode_heic(data),
        _ => decode_raster_oriented(data),
    }
}

/// Decode a raster image via the `image` crate, baking EXIF orientation.
fn decode_raster_oriented(data: &[u8]) -> Result<DynamicImage, String> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("failed to guess raster format: {e}"))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("failed to open raster decoder: {e}"))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("failed to decode raster image: {e}"))?;
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    Ok(img)
}

/// Decode a HEIC/HEIF image to an RGBA [`DynamicImage`].
///
/// `heif-oxide` applies the container's orientation transforms and delivers
/// display-ready sRGB, so no further rotation is needed.
fn decode_heic(data: &[u8]) -> Result<DynamicImage, String> {
    let decoded =
        heif_oxide::decode_bytes(data).map_err(|e| format!("failed to decode heic: {e}"))?;
    let rgba = RgbaImage::from_raw(decoded.width, decoded.height, decoded.to_rgba8())
        .ok_or_else(|| "heic decoded to a buffer that does not match its size".to_string())?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

/// Parse SVG bytes into a `usvg::Tree` with system fonts loaded.
fn parse_svg(data: &[u8]) -> Result<usvg::Tree, String> {
    let mut options = usvg::Options::default();
    Arc::make_mut(&mut options.fontdb).load_system_fonts();
    usvg::Tree::from_data(data, &options).map_err(|e| format!("failed to parse svg: {e}"))
}

/// Rasterize an SVG at a target pixel resolution, preserving aspect ratio.
/// The resulting image will fit within `target_px_w × target_px_h` pixels.
fn rasterize_svg_at_size(
    data: &[u8],
    target_px_w: u32,
    target_px_h: u32,
) -> Result<DynamicImage, String> {
    let tree = parse_svg(data)?;
    let svg_size = tree.size();

    // Compute a uniform scale that fits the SVG within the target pixel
    // bounds while preserving aspect ratio.
    let scale = (target_px_w as f32 / svg_size.width()).min(target_px_h as f32 / svg_size.height());

    let out_w = (svg_size.width() * scale).ceil() as u32;
    let out_h = (svg_size.height() * scale).ceil() as u32;
    let out_w = out_w.max(1);
    let out_h = out_h.max(1);

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
        .ok_or_else(|| "svg dimensions are too large to rasterize".to_string())?;

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let image = RgbaImage::from_raw(out_w, out_h, pixmap.take())
        .ok_or_else(|| "failed to build raster image from svg".to_string())?;

    Ok(DynamicImage::ImageRgba8(image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;
    use image::imageops::FilterType;
    use ratatui_image::Resize;

    #[test]
    fn rasterize_svg_at_size_produces_correct_dimensions() {
        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='blue'/></svg>"#;
        let image = rasterize_svg_at_size(svg, 200, 100).expect("should rasterize");
        let (w, h) = image.dimensions();
        assert_eq!(w, 200, "width constrained by target width");
        assert_eq!(h, 100, "height constrained proportionally");
    }

    #[test]
    fn rasterize_svg_at_size_preserves_aspect_ratio() {
        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='green'/></svg>"#;
        let image = rasterize_svg_at_size(svg, 100, 100).expect("should rasterize");
        let (w, h) = image.dimensions();
        assert_eq!(w, 100, "width constrained by target width");
        assert_eq!(h, 50, "height preserves 2:1 aspect ratio");
    }

    #[test]
    fn decode_image_accepts_svg() {
        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>"#;
        let metadata = ImageMetadata {
            mime_type: "image/svg+xml".to_string(),
            width: 4,
            height: 3,
            byte_len: svg.len() as u64,
            alt: None,
        };
        let result = decode_image(&metadata, svg, 40, 30);
        assert!(result.is_ok(), "SVG should decode successfully");
    }

    #[test]
    fn decode_image_rejects_invalid_raster_bytes() {
        let metadata = ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 3,
            alt: None,
        };
        let result = decode_image(&metadata, &[1, 2, 3], 1, 1);
        assert!(result.is_err(), "invalid PNG bytes should fail");
    }

    #[test]
    fn decode_image_routes_heic_mime_to_heif_decoder() {
        // An `image/heic` mime must route to the heif-oxide path (which rejects
        // non-HEIF bytes), NOT the image-crate raster path.
        let metadata = ImageMetadata {
            mime_type: "image/heic".to_string(),
            width: 1,
            height: 1,
            byte_len: 3,
            alt: None,
        };
        let result = decode_image(&metadata, &[1, 2, 3], 1, 1);
        assert!(
            result.is_err(),
            "non-HEIF bytes should fail via the heif path"
        );
        assert!(
            result.unwrap_err().contains("heic"),
            "error should come from the heif decoder"
        );
    }

    #[test]
    fn decode_raster_oriented_accepts_raster_and_preserves_dimensions() {
        // A valid PNG with no EXIF orientation decodes to the same dimensions
        // (the orientation path must be a no-op for default orientation).
        let buf = image::RgbaImage::from_fn(4, 3, |x, y| {
            image::Rgba([x as u8 * 60, y as u8 * 80, 0, 255])
        });
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let img = decode_raster_oriented(&png.into_inner()).expect("valid PNG should decode");
        assert_eq!(img.dimensions(), (4, 3));
    }

    #[test]
    fn process_job_sends_result_with_encoded_protocol() {
        let picker = Picker::halfblocks();
        let (job_tx, job_rx) = channel::unbounded();
        let (result_tx, result_rx) = channel::unbounded();

        let handle = std::thread::spawn(move || {
            if let Ok(job) = job_rx.recv() {
                process_job(&picker, &result_tx, job);
            }
        });

        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>"#;
        job_tx
            .send(ImageJob {
                id: 42,
                data: Arc::from(svg.to_vec()),
                metadata: ImageMetadata {
                    mime_type: "image/svg+xml".to_string(),
                    width: 4,
                    height: 3,
                    byte_len: svg.len() as u64,
                    alt: None,
                },
                cell_size: Size::new(20, 16),
                resize: Resize::Scale(Some(FilterType::Lanczos3)),
            })
            .expect("send job");

        let result = result_rx.recv().expect("should receive a result");
        assert_eq!(result.id, 42, "result id matches job id");
        assert!(
            result.protocol.is_some(),
            "successful encoding should produce a protocol"
        );
        handle.join().expect("worker thread");
    }

    #[test]
    fn process_job_sends_failure_on_corrupt_image() {
        let picker = Picker::halfblocks();
        let (job_tx, job_rx) = channel::unbounded();
        let (result_tx, result_rx) = channel::unbounded();

        let handle = std::thread::spawn(move || {
            if let Ok(job) = job_rx.recv() {
                process_job(&picker, &result_tx, job);
            }
        });

        job_tx
            .send(ImageJob {
                id: 99,
                data: Arc::from(vec![1, 2, 3]),
                metadata: ImageMetadata {
                    mime_type: "image/png".to_string(),
                    width: 1,
                    height: 1,
                    byte_len: 3,
                    alt: None,
                },
                cell_size: Size::new(20, 16),
                resize: Resize::Scale(Some(FilterType::Lanczos3)),
            })
            .expect("send job");

        let result = result_rx
            .recv()
            .expect("should receive a result even on failure");
        assert_eq!(result.id, 99, "result id matches job id");
        assert!(
            result.protocol.is_none(),
            "failed encoding should have no protocol"
        );
        assert!(
            result.cell_size.width > 0,
            "cell_size should still be populated on failure"
        );
        handle.join().expect("worker thread");
    }
}
