//! Background image-encoding worker.
//!
//! Terminal image protocols are expensive to encode, so the TUI offloads them
//! to a single worker thread: the UI thread submits an [`ImageJob`] and later
//! drains [`ImageResult`]s, keeping the render path free of blocking encodes.
//! A job whose bytes fail to decode still yields a result (with
//! `protocol: None`) so the caller can clear its pending marker and retry at a
//! later frame rather than re-submitting every frame.

use choreo_proto::ImageMetadata;
use crossbeam_channel as channel;
use image::{DynamicImage, RgbaImage};
use ratatui::layout::Size;
use ratatui_image::{Resize, ResizeEncodeRender, picker::Picker, protocol::StatefulProtocol};
use resvg::{tiny_skia, usvg};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Identifier pairing an [`ImageJob`] with the [`ImageResult`] it produces.
pub type ImageId = usize;

/// A job submitted to the background image worker.
pub struct ImageJob {
    /// Identifier echoed back in the matching [`ImageResult`], assigned by
    /// [`next_job_id`] so the UI can pair a result with its request.
    pub id: ImageId,
    /// Raw image bytes, shared via Arc so no deep copy is needed when
    /// submitting the same image at multiple sizes (inline + fullscreen).
    pub data: Arc<[u8]>,
    /// Source image metadata (mime type, declared dimensions, alt text).
    pub metadata: ImageMetadata,
    /// Terminal cell size to encode for; one job is submitted per size.
    pub cell_size: Size,
    /// Resize strategy applied when encoding to `cell_size`.
    pub resize: Resize,
}

/// The result of a completed encoding job.
///
/// `protocol` is `Some` on success and `None` on failure.  The caller should
/// always clear `pending_job` on receipt so the image can be re-attempted
/// on a subsequent frame.
pub struct ImageResult {
    /// Identifier of the [`ImageJob`] this result answers.
    pub id: ImageId,
    /// The encoded protocol on success, or `None` when decoding/encoding failed.
    pub protocol: Option<StatefulProtocol>,
    /// Cell size the job was submitted for; populated even on failure so the
    /// caller can record exactly which size failed.
    pub cell_size: Size,
}

/// The image worker: its job/result channels and the backing thread.
///
/// Dropping the worker drops `job_tx`, which ends the worker thread's `recv`
/// loop; join `handle` to wait for it to drain and exit.
pub struct ImageWorker {
    /// Submits encoding jobs to the worker thread.
    pub job_tx: channel::Sender<ImageJob>,
    /// Receives completed encoding results on the UI thread.
    pub result_rx: channel::Receiver<ImageResult>,
    /// The background thread running the encode loop; it exits once `job_tx`
    /// is dropped, so joining it observes clean shutdown.
    pub handle: thread::JoinHandle<()>,
}

static NEXT_JOB_ID: AtomicUsize = AtomicUsize::new(1);

/// Allocate the next unique [`ImageId`], shared across every worker.
///
/// Ids only need to be unique within the process (they pair a job with its
/// result), so a relaxed atomic counter suffices — the id synchronizes no
/// other memory.
pub fn next_job_id() -> ImageId {
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

impl ImageWorker {
    /// Spawn a background thread that processes image encoding jobs.
    ///
    /// The worker owns a clone of `picker` and uses it to create and encode
    /// terminal protocols from raw image bytes.
    #[must_use]
    pub fn spawn(picker: Picker) -> Self {
        let (job_tx, job_rx) = channel::unbounded::<ImageJob>();
        let (result_tx, result_rx) = channel::unbounded::<ImageResult>();

        let handle = thread::spawn(move || {
            tracing::info!("[choreo-tui] image worker started");
            loop {
                match job_rx.recv() {
                    Err(_) => {
                        tracing::info!("[choreo-tui] image worker shutting down");
                        break;
                    }
                    Ok(job) => process_job(&picker, &result_tx, &job),
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
/// Every outcome produces a result — decode/encode errors and a panicked
/// encoder alike yield `protocol: None` — so the caller always clears its
/// pending marker and can retry at a later frame instead of waiting forever.
fn process_job(picker: &Picker, result_tx: &channel::Sender<ImageResult>, job: &ImageJob) {
    tracing::debug!(
        "[choreo-tui] image worker processing job {} ({} {}x{})",
        job.id,
        job.metadata.mime_type,
        job.metadata.width,
        job.metadata.height,
    );

    let protocol = match guard(|| encode_job(picker, job)) {
        Ok(protocol) => Some(protocol),
        Err(failure) => {
            // One log line covers both failure kinds (a decode/encode error and
            // a caught panic), so a broken image is visible in the log at the
            // default level rather than only at `trace`.
            tracing::warn!(
                "[choreo-tui] image worker failed on job {} ({} {}x{}): {failure}",
                job.id,
                job.metadata.mime_type,
                job.metadata.width,
                job.metadata.height,
            );
            None
        }
    };

    let result = ImageResult {
        id: job.id,
        protocol,
        cell_size: job.cell_size,
    };

    if let Err(e) = result_tx.send(result) {
        tracing::error!(
            "[choreo-tui] image worker failed to send result for {}: {e}",
            job.id
        );
    }
}

/// Decode and encode one job — the span a panic must not be allowed to escape.
///
/// Split out from [`process_job`] so [`guard`] wraps exactly this step: the
/// decode, the protocol construction, and the terminal-protocol encode.
fn encode_job(picker: &Picker, job: &ImageJob) -> Result<StatefulProtocol, String> {
    let font_size = picker.font_size();
    // u16 -> u32 is provably lossless, so `From` is preferred over `as`
    // (an `as` cast would silently truncate if the cell-size type ever
    // widened/shrunk).
    let target_px_w = u32::from(job.cell_size.width).saturating_mul(u32::from(font_size.width));
    let target_px_h = u32::from(job.cell_size.height).saturating_mul(u32::from(font_size.height));

    let image = decode_image(&job.metadata, &job.data, target_px_w, target_px_h)?;

    let mut protocol = picker.new_resize_protocol(image);

    // Trigger terminal-protocol-specific encoding (the expensive part),
    // which also caches the layout dimensions internally.
    protocol.resize_encode(&job.resize, job.cell_size);

    Ok(protocol)
}

/// Run one encode attempt with panics contained, turning a panic into the same
/// `Err` a decode or encode failure produces.
///
/// The worker is a THREAD-LOCAL single point of failure for the whole process:
/// it is one thread shared by every image in every session, so a panic that
/// unwound [`ImageWorker::spawn`]'s receive loop would silently disable inline
/// images until the client restarts — every later job's `send` would be
/// discarded and each slot's `pending_job` marker would never clear. The
/// encoders are third-party code (and the SVG rasteriser) operating on bytes a
/// model chose to read, so containment belongs here rather than in a
/// best-effort `unwrap`-free review of that code.
///
/// `AssertUnwindSafe` is sound for this call: the closure only reads its inputs
/// (`&Picker`, `&ImageJob`), and nothing it mutates is observed after a panic —
/// the panic path yields `Err` and the job is simply reported as failed.
fn guard<T, F>(encode: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(encode)) {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "panicked while encoding: {}",
            panic_message(payload.as_ref())
        )),
    }
}

/// The message carried by a caught panic payload, for the failure log line.
///
/// A panic payload is `Box<dyn Any + Send>`, conventionally either `&'static
/// str` (`panic!("literal")`) or `String` (`panic!("{x}")`). Anything else is
/// reported by kind rather than guessed at.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Decode raw image bytes into a [`DynamicImage`].
///
/// SVG data is rasterised directly at `target_px_w × target_px_h`
/// (preserving aspect ratio). HEIC/HEIF goes through the shared pure-Rust
/// `heifer` decoder (via [`choreo_image::decode_heic`]): it applies the
/// container's own orientation transforms, and the shared probe it starts from
/// enforces the pixel budget on the declared geometry before any decode work,
/// with the container's embedded profile — or its `nclx` description —
/// converted to sRGB. Raster formats use the shared `image`-crate decoder with
/// EXIF orientation baked in, so phone/camera photos render upright.
///
/// The shared [`choreo_image::DecodeError`] is stringified at this boundary
/// because it is the display worker's terminal error point — the only caller logs
/// the message and clears the pending job — and the sibling SVG arm already yields
/// a plain `String`, so one `String` result is the natural type for these mixed
/// sources. A caller that must branch on the failure kind (e.g. treat an
/// over-budget image as a policy rejection) uses the shared crate directly.
fn decode_image(
    metadata: &ImageMetadata,
    data: &[u8],
    target_px_w: u32,
    target_px_h: u32,
) -> Result<DynamicImage, String> {
    match metadata.mime_type.as_str() {
        "image/svg+xml" => rasterize_svg_at_size(data, target_px_w, target_px_h),
        // Stringify the structured decode error to match the SVG arm's `String`;
        // the formatted message carries the same detail to the log line.
        "image/heic" | "image/heif" => choreo_image::decode_heic(data).map_err(|e| e.to_string()),
        _ => choreo_image::decode_raster_oriented(data).map_err(|e| e.to_string()),
    }
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
    // Pixel/area math is intentionally f32: the u32 -> f32 conversion may
    // lose precision for huge dimensions, and the f32 -> u32 ceiling casts
    // can truncate, but both are harmless here — the result is only a
    // raster size, clamped to >= 1 below and bounded by the target box.
    #[expect(clippy::cast_precision_loss)]
    let scale = (target_px_w as f32 / svg_size.width()).min(target_px_h as f32 / svg_size.height());

    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (out_w, out_h) = (
        (svg_size.width() * scale).ceil() as u32,
        (svg_size.height() * scale).ceil() as u32,
    );
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
    use std::io::Cursor;

    #[test]
    fn rasterize_svg_at_size_produces_correct_dimensions() {
        let svg = br"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='blue'/></svg>";
        let image = rasterize_svg_at_size(svg, 200, 100).expect("should rasterize");
        let (w, h) = image.dimensions();
        assert_eq!(w, 200, "width constrained by target width");
        assert_eq!(h, 100, "height constrained proportionally");
    }

    #[test]
    fn rasterize_svg_at_size_preserves_aspect_ratio() {
        let svg = br"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='green'/></svg>";
        let image = rasterize_svg_at_size(svg, 100, 100).expect("should rasterize");
        let (w, h) = image.dimensions();
        assert_eq!(w, 100, "width constrained by target width");
        assert_eq!(h, 50, "height preserves 2:1 aspect ratio");
    }

    #[test]
    fn decode_image_accepts_svg() {
        let svg = br"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>";
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
        // An `image/heic` mime must route to the `heifer` path (which rejects
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
            // Test-gradient values (x < 4, y < 3) always fit in u8; the cast
            // cannot truncate for these dimensions.
            #[expect(clippy::cast_possible_truncation)]
            let px = |v: u32| v as u8;
            image::Rgba([px(x) * 60, px(y) * 80, 0, 255])
        });
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let img = choreo_image::decode_raster_oriented(&png.into_inner())
            .expect("valid PNG should decode");
        assert_eq!(img.dimensions(), (4, 3));
    }

    #[test]
    fn process_job_sends_result_with_encoded_protocol() {
        let picker = Picker::halfblocks();
        let (job_tx, job_rx) = channel::unbounded();
        let (result_tx, result_rx) = channel::unbounded();

        let handle = std::thread::spawn(move || {
            if let Ok(job) = job_rx.recv() {
                process_job(&picker, &result_tx, &job);
            }
        });

        let svg = br"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>";
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
    fn guard_turns_a_panicking_encode_into_a_failure() {
        // The worker is one thread for the whole process, so a panic in the
        // encoder must become an ordinary failed job instead of unwinding the
        // receive loop. Containment also has to be reusable: the next attempt
        // (and the process) must be unaffected.
        let outcome = guard::<u8, _>(|| panic!("encoder exploded"));
        let message = outcome.expect_err("a panic must surface as Err");
        assert!(
            message.contains("panicked while encoding"),
            "the failure names the panic path: {message}"
        );
        assert!(
            message.contains("encoder exploded"),
            "the panic's own message is kept for the log: {message}"
        );

        // A non-string payload is reported by kind rather than lost.
        let outcome = guard::<u8, _>(|| std::panic::panic_any(42u32));
        assert!(
            outcome.unwrap_err().contains("non-string panic payload"),
            "an unusual payload is still described"
        );

        // Ok and Err pass through untouched.
        assert_eq!(guard(|| Ok(7u8)), Ok(7));
        assert_eq!(
            guard(|| Err::<u8, _>("decode failed".to_string())),
            Err("decode failed".to_string())
        );
    }

    #[test]
    fn worker_keeps_processing_after_a_failed_job() {
        // End-to-end over the real worker loop: one undecodable job must not
        // take the thread down, because every later image in every session
        // would then fail silently.
        let ImageWorker {
            job_tx,
            result_rx,
            handle,
        } = ImageWorker::spawn(Picker::halfblocks());
        let svg = br"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='blue'/></svg>";

        job_tx
            .send(ImageJob {
                id: 1,
                data: Arc::from(vec![1u8, 2, 3]),
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
            .expect("send the corrupt job");
        job_tx
            .send(ImageJob {
                id: 2,
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
            .expect("send the valid job");

        // Drop the sender so the worker's receive loop ends and joins cleanly.
        drop(job_tx);

        let first = result_rx.recv().expect("a result for the corrupt job");
        assert_eq!(first.id, 1);
        assert!(first.protocol.is_none(), "the corrupt job fails");

        let second = result_rx.recv().expect("a result for the valid job");
        assert_eq!(second.id, 2);
        assert!(
            second.protocol.is_some(),
            "the loop survived the failed job and encoded the next one"
        );

        handle.join().expect("worker thread exits cleanly");
    }

    #[test]
    fn process_job_sends_failure_on_corrupt_image() {
        let picker = Picker::halfblocks();
        let (job_tx, job_rx) = channel::unbounded();
        let (result_tx, result_rx) = channel::unbounded();

        let handle = std::thread::spawn(move || {
            if let Ok(job) = job_rx.recv() {
                process_job(&picker, &result_tx, &job);
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
