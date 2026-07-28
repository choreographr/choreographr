pub use choreo_client_core::{ClientError, ShellCommand, parse_input_line};
pub use choreo_markdown::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};
use choreo_proto::ImageMetadata;
use image::imageops::FilterType;
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::StatefulProtocol};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Resize filter used for all image rendering and encoding jobs.
pub const IMAGE_RESIZE: Resize = Resize::Scale(Some(FilterType::Lanczos3));

pub use crate::image_worker::ImageId;
use crate::image_worker::ImageResult;

/// A rendered (or pending) image in the chat history.
///
/// Raw image bytes are stored behind an `Arc` so the same data can be sent
/// to the background worker thread for encoding at multiple sizes (inline
/// and fullscreen) without a deep copy of the byte buffer.
pub struct RenderedImage {
    pub metadata: ImageMetadata,
    /// Raw image bytes (SVG or raster).  Kept permanently so the image
    /// can be re-encoded at any display resolution via the worker thread.
    /// Shared via `Arc` to avoid cloning the buffer when submitting jobs.
    pub data: Arc<[u8]>,
    /// Encoded terminal protocols keyed by terminal cell size.
    /// Both inline and fullscreen encodings coexist so toggling between
    /// them is a hashmap lookup — no re-encoding needed.
    pub protocols: HashMap<Size, StatefulProtocol>,
    /// Sizes for which encoding previously failed.
    /// The render path will not re-submit jobs for these sizes.
    pub failed_sizes: HashSet<Size>,
    /// Job ID of a currently-pending encoding request, or `None` when
    /// no encoding is in flight (idle or cached).
    pub pending_job: Option<ImageId>,
}

impl RenderedImage {
    /// Create a placeholder image.  The render path will submit an encoding
    /// job to the background worker when the image becomes visible — no job
    /// is enqueued here so there is nothing to cancel.
    pub fn new_placeholder(metadata: ImageMetadata, data: Arc<[u8]>) -> Self {
        RenderedImage {
            metadata,
            data,
            protocols: HashMap::new(),
            failed_sizes: HashSet::new(),
            pending_job: None,
        }
    }

    /// Apply a completed [`ImageResult`].
    ///
    /// On success the encoded protocol is inserted into the cache keyed by
    /// its cell size — old entries at other sizes are preserved so that
    /// switching between inline and fullscreen never requires re-encoding.
    /// On failure the size is recorded in `failed_sizes` so the render path
    /// does not re-submit a job for that size every frame, but other sizes
    /// remain eligible.
    pub fn apply_result(&mut self, result: ImageResult) {
        if let Some(protocol) = result.protocol {
            self.protocols.insert(result.cell_size, protocol);
        } else {
            self.failed_sizes.insert(result.cell_size);
        }
        self.pending_job = None;
    }
}

pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub mod image_worker;
pub mod terminal_progress;

#[cfg(test)]
mod lib_tests;
