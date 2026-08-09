mod cache;
mod connection;
mod diff_render;
mod markdown_render;
mod render;
mod scrollbar;
mod state;
mod syntax;

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

use anyhow::Context;
use clap::Parser;

/// Shared clap [`Styles`] for this crate's CLI binary.
///
/// Each CLI crate keeps its own copy (choreo-proto is the wire protocol and
/// must not host CLI styling); if this ever grows, promote it to a dedicated
/// micro-crate instead of putting it in choreo-proto.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser)]
// Bare `version` makes `--version` print the crate version (CARGO_PKG_VERSION);
// clap handles it before the app starts, so it works headless too.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreo-tui",
    version,
    about = "Choreographr terminal UI",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Connect via TCP/Noise IK at this address (e.g. 127.0.0.1:9443)
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Path to the server's Noise IK public key (defaults to ~/.config/choreographr/transport.pub)
    #[arg(long = "server-pk")]
    server_pk: Option<String>,
}

/// Entry point for the `choreo-tui` binary wrapper.
///
/// The root `choreographr` package declares `choreo-tui` as a thin binary
/// that calls this function, so the TUI lives entirely in this library crate.
pub fn main() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let cli = Cli::parse();

    let mode = if let Some(addr) = cli.tcp_addr {
        let server_pk = choreo_client_core::read_server_pk(cli.server_pk.as_deref())
            .context("failed to read server public key")?;
        choreo_client_core::ConnectionMode::Tcp { addr, server_pk }
    } else {
        choreo_client_core::ConnectionMode::UnixSocket(choreo_proto::socket_path())
    };

    let log_path = format!("/tmp/choreo-tui-{}.log", std::process::id());
    let log_file = std::fs::File::create(&log_path)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(log_file)
        .with_ansi(false);
    tracing_subscriber::registry().with(file_layer).init();

    connection::run_app(mode)?;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match Cli::try_parse_from(["choreo-tui", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod lib_tests;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod test_util;
