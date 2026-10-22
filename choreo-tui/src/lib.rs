//! Terminal user interface for the Choreographr daemon.
//!
//! `choreo-tui` renders an interactive chat client over `ratatui`/`crossterm`
//! on top of `choreo-client-core`'s daemon connection. All of the TUI's logic
//! lives in this library crate; the shipped `choreo-tui` binary (`src/main.rs`)
//! is a thin wrapper that calls [`main`], so the connection and session logic
//! stays unit-testable without a real terminal.
//!
//! The public surface is intentionally small: the entry point ([`main`]), the
//! background image-encoding [`image_worker`], the [`RenderedImage`] model
//! shared with the render path, its [`IMAGE_RESIZE`] filter, and
//! [`build_picker`]. Everything else — rendering, selection, syntax
//! highlighting, connection handling, and all direct-to-stdout terminal OSC
//! emission (the crate-private `terminal` module) — is crate-private.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

// `pub`: the poll/dial helpers are exercised from tests/it/autostart_poll.rs
// (the integration suite — real waits and real sockets must not live in the
// unit-test module per AGENTS.md), which needs cross-crate visibility.
// `#[doc(hidden)]`: it is an implementation detail, not part of the public
// API contract — the re-exports exist only for that test target.
#[doc(hidden)]
pub mod autostart;
mod backend;
mod cache;
mod connection;
mod diff_render;
mod markdown_render;
mod render;
mod scrollbar;
mod selection;
mod state;
mod syntax;
mod terminal;
mod text_filter;

pub use choreo_client_core::{ClientError, Command, parse_input_line};
pub use choreo_markdown::{
    ListItem, MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline,
};
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
    /// Source image metadata (mime type, declared dimensions, alt text).
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
    /// True while an on-demand `ClientMessageType::GetImage` for this image is in
    /// flight (bytes stripped from the turn snapshot, protocol v6). Dedupes
    /// the fetch so the per-frame render path cannot re-request the same image
    /// while a reply is pending.
    pub fetching: bool,
    /// True once a fetch came back `None` (not found — deleted/evicted). The
    /// render path then stops re-requesting, so a missing image cannot spin a
    /// fetch every frame; it renders as an empty placeholder.
    pub fetch_failed: bool,
}

impl RenderedImage {
    /// Create a placeholder image.  The render path will submit an encoding
    /// job to the background worker when the image becomes visible — no job
    /// is enqueued here so there is nothing to cancel.
    #[must_use]
    pub fn new_placeholder(metadata: ImageMetadata, data: Arc<[u8]>) -> Self {
        RenderedImage {
            metadata,
            data,
            protocols: HashMap::new(),
            failed_sizes: HashSet::new(),
            pending_job: None,
            fetching: false,
            fetch_failed: false,
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

/// Build the terminal image picker, probing the terminal's graphics
/// capabilities and falling back to half-block rendering when the query fails
/// (for example, when stdout is not a TTY).
#[must_use]
pub fn build_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
}

pub mod image_worker;

use choreo_proto::ProtoError;

/// The quit message to print after the TUI exits with a connection error.
/// The only case that needs special wording is a protocol-version mismatch:
/// the daemon and the TUI were built against incompatible wire protocol
/// versions, and the actionable fix is restarting the daemon (an older build
/// is still running). Everything else keeps the generic `{error}` rendering.
pub(crate) fn connection_quit_message(error: &ClientError) -> String {
    match error {
        ClientError::Proto(ProtoError::UnsupportedVersion { .. }) => {
            "connection to the daemon failed: the daemon's protocol version is incompatible — \
             restart the daemon (it may be an older build)"
                .to_string()
        }
        _ => format!("connection to the daemon failed: {error}"),
    }
}

use anyhow::Context;
use choreo_shared::clap_styles;
use choreo_shared::logging::{ConsoleSink, LogOptions, Verbosity};
use clap::Parser;

#[derive(Parser)]
// `--version` prints the crate version (CARGO_PKG_VERSION) with the release
// name appended via `choreo_shared::release_name` — e.g. `0.2.0 (Lindy)`, or the
// bare version when the name file is empty. clap handles it before the app
// starts, so it works headless too.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreo-tui",
    version = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION")),
    about = "Choreographr terminal UI",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    // Increase logging verbosity (-v debug, -vv trace)
    #[command(flatten)]
    verbosity: Verbosity,

    /// Run the instance out of this base directory instead of the platform
    /// defaults: `{base}/config`, `{base}/data`, `{base}/run` (the socket),
    /// and `{base}/log`. Equivalent to exporting `CHOREOGRAPHR_BASE_DIR`; the
    /// TUI-spawned daemon inherits it.
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<std::path::PathBuf>,

    /// Write the TUI's log file to this path instead of the default
    /// (`{base}/log/tui-<pid>.log`, else `$XDG_STATE_HOME/choreographr`, else
    /// the platform temp dir). The TUI owns the terminal, so it never mirrors
    /// to stderr — diagnostics are read from this file. This only chooses the
    /// path; it never affects RUST_LOG/-v/-q level selection.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Connect via TCP/Noise IK at this address (e.g. 127.0.0.1:9443)
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Path to the server's Noise IK public key (defaults to the pinned key
    /// in `known_servers.toml`; on first contact the key is learned via the
    /// Noise XX handshake and must be confirmed interactively)
    #[arg(long = "server-pk")]
    server_pk: Option<String>,

    /// Pre-approve the server fingerprint for first contact (headless mode:
    /// the fingerprint, with or without spaces, must match exactly —
    /// including letter case — the key the daemon presents during the XX
    /// probe; compare against the value the daemon operator reads out with
    /// 'choreographr fingerprint'). Ignored when --server-pk is given.
    #[arg(long = "trust-fingerprint")]
    trust_fingerprint: Option<String>,
}

/// Resolve the connection mode for a TCP address, implementing the trust
/// flow. Resolution order:
///
/// 1. explicit `--server-pk` file — operator-provided pin, used directly
///    (IK against it; this is the original out-of-band mechanism and wins
///    over everything);
/// 2. a pin in `known_servers.toml` — IK against the pinned key, with the
///    loud re-pair guidance on handshake failure (`TcpPinned`);
/// 3. otherwise FIRST CONTACT: probe the daemon's key with the Noise XX
///    handshake and require the human to confirm its fingerprint (either
///    `expected_fingerprint` for headless use, or an interactive y/N prompt
///    — this runs in `main()` while the terminal is still in cooked mode, so
///    the prompt is a plain stdout/stdin exchange, no TUI involved) before
///    pinning and connecting.
///
/// This runs BEFORE the TUI starts, so no `Unlock` — which carries the
/// daemon's private key — can ever flow over an unconfirmed channel.
///
/// Every TCP path here ALSO ends with an authorization preflight
/// ([`preflight_daemon_authorization`]): a full Noise IK handshake that the
/// daemon's ACL judges. This is what DENIES the TUI from starting when this
/// client's key is not enrolled — the XX probe used for first contact CANNOT
/// detect that (its handshake completes client-side before the daemon's ACL
/// check runs), so without the preflight an un-enrolled client used to get
/// all the way into the TUI and only fail later with a cryptic I/O error.
fn resolve_connect_mode(
    addr: &str,
    server_pk_path: Option<&str>,
    expected_fingerprint: Option<&str>,
) -> anyhow::Result<choreo_client_core::ConnectionMode> {
    use choreo_client_core::ConnectionMode;

    // 1. Explicit operator-provided key file wins over the store.
    if let Some(path) = server_pk_path {
        let pk = choreo_client_core::read_server_pk(Some(path))
            .context("failed to read server public key")?;
        tracing::info!(addr, path, "using explicitly provided server public key");
        preflight_daemon_authorization(addr, &pk)?;
        return Ok(ConnectionMode::Tcp {
            addr: addr.to_string(),
            server_pk: pk,
        });
    }

    // 2. A pinned key from a previous confirmed first contact.
    let mut known = choreo_client_core::KnownServers::load()?;
    if let Some(pk) = known.lookup(addr)? {
        tracing::info!(
            addr,
            fingerprint = %choreo_client_core::fingerprint(&pk),
            "connecting with pinned server key"
        );
        preflight_daemon_authorization(addr, &pk)?;
        return Ok(ConnectionMode::TcpPinned(addr.to_string()));
    }

    // 3. First contact: learn the key, confirm the fingerprint, pin.
    let learned = choreo_client_core::probe_server_key(addr)
        .context("first-contact probe failed (is the daemon running with --tcp-addr, and is this client's key enrolled in its ACL?)")?;
    let fp = choreo_client_core::fingerprint(&learned);

    let confirmed = if let Some(expected) = expected_fingerprint {
        // Headless: compare against the pre-approved fingerprint (whitespace
        // is insignificant — accept either the grouped or the plain form).
        let matches = strip_fp_whitespace(expected) == strip_fp_whitespace(&fp);
        if !matches {
            tracing::error!(
                addr,
                expected = %expected,
                actual = %fp,
                "--trust-fingerprint does not match the server's key"
            );
        }
        matches
    } else {
        // Interactive: ask the human. The daemon operator should have sent
        // this fingerprint out-of-band in the enrollment conversation.
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout();
        confirm_first_contact(addr, &fp, &mut stdin, &mut stdout)?
    };

    if !confirmed {
        anyhow::bail!(
            "first contact with {addr} not confirmed — refusing to connect; \
             verify the daemon's fingerprint with its operator and retry"
        );
    }

    known
        .pin(addr, &learned)
        .context("failed to persist the confirmed server key")?;
    tracing::info!(addr, fingerprint = %fp, "server key confirmed and pinned");
    preflight_daemon_authorization(addr, &learned)?;
    Ok(ConnectionMode::Tcp {
        addr: addr.to_string(),
        server_pk: learned,
    })
}

/// Preflight the daemon's authorization decision: run a full Noise IK
/// handshake against the server key we are about to connect with, so the
/// daemon's ACL verdict is known BEFORE the TUI starts. On refusal this
/// exits with a remediation message instead of letting the TUI start and
/// die on its first prompt.
fn preflight_daemon_authorization(addr: &str, server_pk: &[u8; 32]) -> anyhow::Result<()> {
    if let Err(e) = choreo_client_core::verify_daemon_authorization(addr, server_pk) {
        // The enrollment-remediation message embeds THIS client's identity,
        // so a TUI-only user (who has no choreographr binary) can copy-paste
        // their key straight to the daemon operator. Reading the keypair is
        // best-effort: it exists whenever the preflight ran, and a failure
        // degrades to the generic wording rather than masking the real
        // error.
        let client_pk = choreo_client_core::own_transport_pubkey().ok();
        anyhow::bail!(preflight_failure_message(addr, &e, client_pk.as_ref()));
    }
    Ok(())
}

/// Render a preflight failure as a human-actionable message. Factored out
/// of [`preflight_daemon_authorization`] so tests can pin the wording.
fn preflight_failure_message(
    addr: &str,
    err: &choreo_client_core::PreflightError,
    client_pk: Option<&[u8; 32]>,
) -> String {
    match err {
        choreo_client_core::PreflightError::Unreachable(io) => format!(
            "cannot reach the daemon at {addr}: {io}\n\
             is the daemon running, and did it start with --tcp-addr?",
        ),
        choreo_client_core::PreflightError::Rejected(_) => {
            // The identity block: fingerprint for human comparison, plus the
            // raw base64 pubkey the operator's enrollment command consumes —
            // everything the TUI-only user needs, nothing left for them to
            // run on this machine.
            let identity = client_pk.map_or_else(
                || {
                    "your client key could not be read from this machine's config directory.\n"
                        .to_string()
                },
                |pk| {
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(pk);
                    format!(
                        "your client fingerprint:\n\
                         {}\n\
                         your public key (send this to the daemon operator):\n\
                         {b64}\n\
                         the operator enrolls it on the daemon machine with:\n\
                         choreographr acl-add {b64}\n\
                         (or '/acl add {b64}' from a local TUI connection there)\n",
                        choreo_client_core::fingerprint(pk),
                    )
                },
            );
            format!(
                "the daemon at {addr} did not authorize this client's transport key.\n\
                 {identity}\
                 once enrolled, reconnect.\n\
                 if you pinned this server before and its key has since changed, remove the\n\
                 entry for {addr} from known_servers.toml and reconnect to re-confirm.\n\
                 ({err})",
            )
        }
    }
}

/// Normalize a fingerprint for comparison: drop the grouping spaces, keep
/// case EXACTLY. Base64's standard alphabet is case-sensitive, and the
/// fingerprint gate is the only thing standing between a first-contact MITM
/// and the daemon's private key — so the comparison must not widen the
/// match: a case-insensitive compare would let an attacker whose key
/// differs from the real one only in letter case pass the check after
/// ~2^28 key-generation tries. Whitespace stays insignificant (copy-paste
/// with or without the 4-char grouping must both work), but any case
/// difference is a mismatch — a transcribed-but-mistyped fingerprint
/// SHOULD be refused and re-read rather than fuzzily accepted.
fn strip_fp_whitespace(fp: &str) -> String {
    fp.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Interactive first-contact confirmation: render the fingerprint, ask for
/// y/N, read the answer. Factored with injected I/O so tests can drive it
/// without a real terminal.
fn confirm_first_contact(
    addr: &str,
    fingerprint: &str,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    writeln!(output, "First contact with {addr}.")?;
    writeln!(output, "Server fingerprint:")?;
    writeln!(output, "  {fingerprint}")?;
    writeln!(
        output,
        "Compare this with the fingerprint the daemon operator sent you,"
    )?;
    writeln!(output, "then trust and pin this server? [y/N]")?;
    output.flush()?;

    let mut line = String::new();
    let n = input
        .read_line(&mut line)
        .context("failed to read the trust confirmation answer")?;
    if n == 0 {
        // EOF on stdin (piped/redirected input without an answer): treat as
        // a refusal — an absent human never confirms trust.
        tracing::warn!(
            addr,
            "stdin closed during first-contact confirmation; refusing"
        );
        return Ok(false);
    }
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Entry point for the `choreo-tui` binary wrapper.
///
/// The root `choreographr` package declares `choreo-tui` as a thin binary
/// that calls this function, so the TUI lives entirely in this library crate.
///
/// # Errors
///
/// Returns an error when CLI argument parsing fails, the TCP connection
/// mode cannot be resolved (server-key probe, fingerprint confirmation, or
/// daemon-authorization preflight), or the TUI application itself fails
/// while running.
pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Apply the `--base-dir` override before ANY path is resolved (the log file
    // and, for a TCP connect, the transport keypair + known_servers). Setting
    // the process env also propagates it to the daemon this TUI autostarts, so
    // the child shares the instance root.
    choreo_shared::paths::set_base_dir_from_cli(cli.base_dir.clone());

    // Logging is initialized FIRST — before the TCP trust flow below, which
    // emits `tracing` events (this mirrors the daemon, whose logging init is
    // its very first act). The TUI owns the terminal (the alternate screen), so
    // it installs NO console mirror — diagnostics go to a hardened, pid-keyed
    // file only, through the shared initializer (the same level policy every
    // binary uses: `-v`/`-q` flags win over `RUST_LOG`).
    let _ = choreo_shared::logging::init(LogOptions {
        binary: "tui",
        verbosity: cli.verbosity,
        log_file: cli.log_file.as_deref(),
        console: ConsoleSink::None,
        with_target: true,
        extra_directives: &[],
    });

    let mode = if let Some(addr) = cli.tcp_addr {
        resolve_connect_mode(
            &addr,
            cli.server_pk.as_deref(),
            cli.trust_fingerprint.as_deref(),
        )?
    } else {
        // Unix-socket mode: NO pre-flight probe. The TUI connects directly;
        // when the dial itself finds nothing listening, the connection task
        // (connection::run_app) autostarts a daemon via autostart::start_daemon
        // and retries. The TCP branch above never autostarts — a remote daemon
        // is not launchable from the client machine, by definition.
        choreo_client_core::ConnectionMode::UnixSocket(choreo_proto::socket_path())
    };

    // Best-effort startup banner carrying the release name
    // (choreo-shared/release-name.txt) alongside the crate version. Logging here
    // is deliberately best-effort: when the shared initializer found no writable
    // log file (and the console sink is `None`), no subscriber is installed and
    // this event is simply dropped.
    tracing::info!(
        version = %choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION")),
        "choreo-tui starting"
    );

    connection::run_app(mode)?;
    Ok(())
}

#[cfg(test)]
mod tests;
