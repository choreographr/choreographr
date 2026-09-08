mod client;
mod components;
mod hooks;
mod render;
mod state;

use crate::client::apply_daemon_message;
use crate::components::{Composer, HistoryList, Toolbar};
use crate::hooks::use_daemon_connection;
use crate::state::{AppState, UiEvent};
use choreo_client_core::{ConnectionMode, read_server_pk};
use choreo_proto::socket_path;
use clap::Parser;
use dioxus::prelude::*;
use futures_util::StreamExt as _;
use std::sync::OnceLock;

/// Global connection mode, set once at startup from CLI args.
static CONNECTION_MODE: OnceLock<ConnectionMode> = OnceLock::new();

/// Default TCP/Noise-IK daemon address for iOS's DEGRADED fallback mode: the
/// remote daemon the embedded daemon degrades to when in-process construction
/// fails (see `embedded_connection_mode`). The Android build keeps the
/// Unix-socket default because Termux exposes one. `TcpPinned` resolves the
/// server key from `known_servers.toml` at connect time, so the pin lives in
/// the app's own config dir (the iOS sandbox) and no
/// `~/.config/.../transport.pub` file — which the sandbox cannot read — is
/// needed. The same-address pin can be established by any choreographr client
/// on the same host (TUI first-use flow); until then the connect fails loudly
/// with the re-pair guidance.
// Desktop/Android never reference the fallback constant (only the iOS body of
// `default_connection_mode` does), so silence the dead_code warning there —
// the constant is deliberately shared, not cfg-duplicated.
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
const IOS_DEFAULT_TCP_ADDR: &str = "127.0.0.1:9443";

// ── iOS embedded daemon ────────────────────────────────────────────────────
//
// On iOS (and ONLY iOS — see the `cfg` below; the desktop and Android builds
// never compile or link any of this) the GUI runs the daemon in-process:
// `DaemonState::open` with the sandbox-safe `choreo_daemon::ToolPolicy::Mobile`
// (no shell/exec/RISC-V tools, no MCP subprocess spawning) feeds
// `choreo_daemon::spawn_embedded`, whose `EmbeddedLink` ends become the
// `ConnectionMode::InProcess` channel pair. Messages travel as values — no
// codec, no socket — and the same `ClientConn` state machine serves the
// connection that the Unix/TCP transports use.
//
// The construction is fallible (DB open, catalog paths): every failure is
// logged and degrades to the previous `TcpPinned` remote-daemon mode, which
// remains the fallback path. No `unwrap`/`expect`/`panic` anywhere — the GUI
// must launch even when the embedded daemon cannot.

/// The kept-alive embedded daemon (iOS only).
///
/// Why a process-lifetime static and not a Drop'd owner: the Dioxus Native
/// lifecycle has no daemon-shutdown hook — the event loop runs until the
/// process dies, so there is no natural point to call
/// `choreo_daemon::EmbeddedDaemon::shutdown` (which consumes `self`) and no
/// graceful-drain window to await anyway. Deliberately: shutdown happens at
/// process teardown; the `Drop` warn in `choreo_daemon::embedded` ("dropped
/// without shutdown") is EXPECTED there and harmless — iOS reaps the whole
/// process. No polling and no background shutdown thread is added for this.
///
/// Why a `Mutex<Option<_>>` and not a `OnceLock`: the underlying
/// `EmbeddedDaemon` is now `Sync` (its JoinHandle ferry is a crossbeam
/// channel, whose ends are `Sync`), but the OPTION is the point: on the
/// (never-expected, but reachable) double-startup path, a `OnceLock` would
/// have to DROP the freshly-spawned daemon detached — the Drop warn's
/// "core left detached" defect, with the stale daemon ALSO left immortal.
/// With `Mutex<Option<_>>` the double-startup path can instead do the
/// RIGHT thing: `shutdown()` the stale daemon (ordered drain: ShuttingDown
/// broadcast, command-loop join, bounded connection-thread joins) and store
/// the new one whose link the session is about to use.
#[cfg(target_os = "ios")]
static EMBEDDED_DAEMON: std::sync::Mutex<Option<choreo_daemon::EmbeddedDaemon>> =
    std::sync::Mutex::new(None);

/// Build the embedded-daemon `ConnectionMode::InProcess` (iOS only).
///
/// Returns `None` (after logging) on any construction failure; the caller
/// falls back to `IOS_DEFAULT_TCP_ADDR`'s `TcpPinned` remote-daemon mode so
/// the app still launches.
#[cfg(target_os = "ios")]
fn embedded_connection_mode() -> Option<ConnectionMode> {
    // Standard path resolvers (db/accounts/catalog), same convention the CLI
    // daemon uses: `dirs` resolves inside the iOS app sandbox (app-container
    // HOME), so the embedded daemon's database lives entirely in the app's
    // own storage — never the shared `~/.config/choreographr` the socket
    // daemon uses (which the sandbox cannot see).
    let db_path = match choreo_daemon::db::db_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "embedded daemon: cannot resolve db path; \
                falling back to TcpPinned remote daemon");
            return None;
        }
    };
    let accounts_path = match choreo_daemon::accounts::accounts_config_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "embedded daemon: cannot resolve accounts path; \
                falling back to TcpPinned remote daemon");
            return None;
        }
    };
    // max_turns 0 = unlimited: the GUI's embedded daemon has no env/config
    // knob surface (the CLI's `resolve_max_turns` is CLI-private), and a
    // GUI-driven agent loop is bounded by its own Stop/cancel affordances.
    let state = match choreo_daemon::DaemonState::open(choreo_daemon::OpenOptions {
        db_path,
        accounts_path,
        catalog_paths: choreo_daemon::catalog::CatalogPaths::from_dirs(),
        tool_policy: choreo_daemon::ToolPolicy::Mobile,
        max_turns: 0,
    }) {
        Ok(state) => state,
        Err(e) => {
            tracing::error!(error = %e, "embedded daemon: DaemonState::open failed; \
                falling back to TcpPinned remote daemon");
            return None;
        }
    };
    let daemon =
        match choreo_daemon::spawn_embedded(state, choreo_daemon::EmbeddedOptions::default()) {
            Ok(daemon) => daemon,
            Err(e) => {
                tracing::error!(error = %e, "embedded daemon: spawn_embedded failed; \
                falling back to TcpPinned remote daemon");
                return None;
            }
        };
    // connect() spawns the connection thread BEFORE returning, so it cannot
    // fail for readiness reasons — only the connection cap (impossible here:
    // this is the first and only link).
    let link = match daemon.connect() {
        Ok(link) => link,
        Err(e) => {
            tracing::error!(error = %e, "embedded daemon: connect failed; \
                falling back to TcpPinned remote daemon");
            // The core is already RUNNING behind this handle (spawn_embedded
            // returned Ok). Dropping it detached would be the Drop-warn defect
            // (no ShuttingDown broadcast, no bounded joins); run the ordered
            // drain instead — there are no connections to wait out yet, so
            // this is prompt (command-loop join + empty drain), and the
            // fallback TcpPinned mode starts from a fully stopped process.
            daemon.shutdown();
            return None;
        }
    };
    // Keep the daemon handle alive for the whole process (see EMBEDDED_DAEMON:
    // no graceful-shutdown hook exists — teardown is process death).
    // Poisoned-mutex fallback per the workspace's error-handling rules (this
    // runs on the single startup thread, so poisoning cannot arise in
    // practice — but never unwrap in production code).
    let mut guard = EMBEDDED_DAEMON.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stale) = guard.take() {
        // Double-startup (never expected: embedded_connection_mode runs once
        // from main() before the event loop, but reachable in principle). The
        // session is about to use the NEW link, so the NEW daemon must be the
        // one that keeps running: drain the stale one through its ordered
        // shutdown, and store the new one. A OnceLock could not do this — it
        // would leak BOTH daemons (see the static's doc).
        tracing::warn!(
            "embedded daemon: EMBEDDED_DAEMON already held a daemon; draining the stale one"
        );
        stale.shutdown();
    }
    *guard = Some(daemon);
    drop(guard);
    tracing::info!("embedded daemon: in-process connection established");
    Some(ConnectionMode::InProcess {
        daemon_tx: link.client_tx,
        daemon_rx: link.daemon_rx,
    })
}

/// Resolve the no-CLI-args connection mode.
///
/// Split out of [`main`] so it is unit-testable on the host. The bodies are
/// selected by `#[cfg]` (not `cfg!`), so each target compiles exactly one:
/// desktop/Android keep the Unix-socket default (pinned by
/// `default_mode_is_unix_socket_on_host`; `cfg!` is compile-time, so an
/// on-host test can only pin the desktop branch — the iOS branch is exercised
/// on-device and by `scripts/check-ios.sh`'s target compile). On iOS the
/// embedded in-process daemon is attempted first, degrading to the `TcpPinned`
/// remote-daemon mode on any construction failure (see
/// [`embedded_connection_mode`]).
fn default_connection_mode() -> ConnectionMode {
    // Desktop + Android: unchanged Unix-socket default. The Android build
    // keeps it because Termux exposes one; desktop always has one.
    #[cfg(not(target_os = "ios"))]
    return ConnectionMode::UnixSocket(socket_path());

    // iOS: no usable Unix-socket daemon path in the sandbox — run the daemon
    // in-process instead; remote TCP/Noise-IK with a pinned key is the
    // degraded fallback.
    #[cfg(target_os = "ios")]
    embedded_connection_mode()
        .unwrap_or_else(|| ConnectionMode::TcpPinned(IOS_DEFAULT_TCP_ADDR.to_string()))
}

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
// Bare `version` wires `--version`/`-V` to CARGO_PKG_VERSION, matching the
// other suite binaries (Homebrew formula test + smoke test rely on it).
// ColorChoice is explicitly Auto (clap's default): color only on a TTY,
// never forced into pipes.
#[command(
    name = "choreo-gui",
    version,
    about = "Choreographr GUI",
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

/// Entry point for the `choreo-gui` UI binary.
///
/// This crate declares its own `choreo-gui` binary target (`src/bin/`), a
/// thin wrapper that calls this function, so `cargo run -p choreo-gui` in the
/// workspace produces the executable directly — the GUI is not part of the
/// root `choreographr` suite package and is not published to crates.io
/// (`publish = false`), so it is built from the workspace tree only.
pub fn main() {
    let cli = Cli::parse();

    let mode = if let Some(addr) = cli.tcp_addr {
        // On iOS there is no `~/.config/choreographr/transport.pub` to read —
        // the app sandbox cannot see it — so an explicit `--tcp-addr` dials in
        // pinned-key mode (pin sourced from the sandbox `known_servers.toml`)
        // instead of failing at startup on a key file that cannot exist.
        if cfg!(target_os = "ios") {
            ConnectionMode::TcpPinned(addr)
        } else {
            let server_pk = match read_server_pk(cli.server_pk.as_deref()) {
                Ok(pk) => pk,
                Err(e) => {
                    eprintln!("failed to read server public key: {e}");
                    std::process::exit(1);
                }
            };
            ConnectionMode::Tcp { addr, server_pk }
        }
    } else {
        default_connection_mode()
    };

    // Store mode globally so the App component can read it.
    let _ = CONNECTION_MODE.set(mode);

    // Platform-agnostic launch facade: under the `native` feature the macro
    // cfg routes this to the Dioxus Native (Blitz) renderer, which serves
    // desktop, Android and iOS — no desktop()/mobile() branching anywhere.
    dioxus::launch(App);
}

// ── Android entry glue ────────────────────────────────────────────────────────
//
// WHY THIS EXISTS: the Android runtime starts a native-activity app at the C
// symbol `android_main` (android-activity's native_app_glue C shim calls it
// from `rust_glue_entry`); nothing in the dioxus-native/blitz dependency tree
// defines it for us. The `native` cfg's `dioxus::launch` → `dioxus_native::
// launch_cfg` path handles the rest of the Android wiring itself: blitz-shell's
// `create_default_event_loop` fetches the JVM `AndroidApp` handle from a global
// slot and feeds it to winit via `EventLoopBuilderExtAndroid::with_android_app`,
// so the ONLY missing piece is this no_mangle trampoline: stash the `AndroidApp`
// in blitz-shell's slot, then run the exact same `main()` the desktop binary
// uses (the `native` renderer serves desktop and Android with one code path —
// there is deliberately no mobile/webview entry here). On Android the process
// is started with no meaningful argv, and every clap arg is optional, so
// `Cli::parse()` resolves to the Unix-socket default connection mode.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)] // edition 2024: no_mangle is an unsafe attribute
fn android_main(app: android_activity::AndroidApp) {
    blitz_shell::set_android_app(app);
    main();
}

// ── iOS entry glue ───────────────────────────────────────────────────────────
//
// WHY THIS EXISTS: unlike Android, iOS does not call a C symbol directly —
// winit 0.30's iOS backend owns the UIKit launch: its `EventLoop::run_app`
// calls UIApplicationMain itself (with None for both the application class
// and the delegate, so no ObjC bootstrap delegate is needed) and asserts
// that UIApplication::sharedApplication is still nil when it does. The host
// bootstrap (ios/main.m) therefore just calls this trampoline from main();
// it must NEVER call UIApplicationMain first, or the assert below fires and
// the app dies at launch. This trampoline is the crate-side half of that
// contract: a C-callable no_mangle entry that runs the exact same `main()`
// the desktop and Android builds use (the `native` renderer serves desktop,
// Android and iOS with one code path — there is deliberately no
// per-platform UI entry here). The connection story differs: the iOS
// sandbox has no usable Unix-socket daemon path, so `main()` resolves to the
// embedded in-process daemon (see `embedded_connection_mode`), degrading to
// TcpPinned on construction failure — without any branching beyond the cfg
// in `default_connection_mode`.
//
// The event loop is constructed on the main thread (main.m calls us from
// main(), satisfying winit's MainThreadMarker requirement), and blitz-shell
// creates the app's window from ApplicationHandler::resumed — the point
// winit's docs require window creation to happen at, after UIApplicationMain
// has done the UIKit init all UI code needs (rust-windowing/winit#1705).
#[cfg(target_os = "ios")]
#[unsafe(no_mangle)] // edition 2024: no_mangle is an unsafe attribute
pub extern "C" fn choreo_gui_ios_main() {
    main();
}

#[component]
fn App() -> Element {
    let (daemon_tx, mut events_rx) = use_daemon_connection();
    let mut state = use_signal(|| {
        let display_path = match CONNECTION_MODE.get() {
            Some(ConnectionMode::UnixSocket(path)) => path.clone(),
            Some(ConnectionMode::Tcp { addr, .. }) => addr.clone(),
            // Pinned mode also dials an address — display it exactly like
            // the explicit-key Tcp variant (the pin itself is not secret).
            Some(ConnectionMode::TcpPinned(addr)) => addr.clone(),
            // In-process (embedded daemon, iOS): no dial address; show the
            // stable user-visible label for the local trust domain (see
            // client.rs's `connection_addr`, which keys the keystore under
            // the distinct "embedded" string so an embedded daemon can never
            // collide with a real unix daemon's binding under socket_path()).
            Some(ConnectionMode::InProcess { .. }) => "embedded daemon".to_string(),
            None => socket_path(),
        };
        AppState::new(display_path)
    });

    let tx = daemon_tx.read().clone();

    use_future({
        let tx = tx.clone();
        move || {
            let tx = tx.clone();
            async move {
                loop {
                    let event = {
                        let mut guard = events_rx.write();
                        match guard.as_mut() {
                            Some(rx) => rx.next().await,
                            None => break,
                        }
                    };

                    let Some(event) = event else {
                        break;
                    };

                    match event {
                        UiEvent::Daemon(message) => {
                            let result = {
                                let mut app_state = state.write();
                                apply_daemon_message(&mut app_state, message, tx.clone())
                            };
                            if let Err(error) = result {
                                state.write().status_texts.push(format!(
                                    "[client] failed to process daemon message: {error}"
                                ));
                            }
                        }
                        UiEvent::ReaderClosed => {
                            state
                                .write()
                                .status_texts
                                .push("daemon connection closed".to_string());
                        }
                        UiEvent::ReaderFailed(error) => {
                            state
                                .write()
                                .status_texts
                                .push(format!("[client] connection error: {error}"));
                        }
                    }
                }
            }
        }
    });

    rsx! {
        document::Style { {APP_CSS} }
        div { class: "app-shell",
            Toolbar { state, tx: daemon_tx }
            HistoryList { state }
            Composer { state, tx: daemon_tx }
        }
    }
}

const APP_CSS: &str = include_str!("style.css");

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
        let err = match Cli::try_parse_from(["choreo-gui", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    /// On the host (desktop), the no-args connection mode must stay the
    /// Unix-socket default — the iOS branch is `#[cfg]`-selected, so an
    /// on-host test can only pin the desktop branch (the iOS branch compiles
    /// on-device and via `scripts/check-ios.sh`'s target compile; `cfg!` is a
    /// compile-time constant either way). Any accidental flip of the cfg
    /// selection shows up here in the regular unit suite.
    #[test]
    fn default_mode_is_unix_socket_on_host() {
        let mode = default_connection_mode();
        assert!(matches!(mode, ConnectionMode::UnixSocket(_)));
    }

    /// Parsing `--tcp-addr` alone must not fail (the value is resolved later,
    /// in `main`): the flag pair stays optional so iOS's argv-less launch
    /// (mirroring the Android note above) parses cleanly.
    #[test]
    fn tcp_addr_alone_parses() {
        let cli = Cli::try_parse_from(["choreo-gui", "--tcp-addr", "192.168.1.20:9443"])
            .unwrap_or_else(|e| panic!("--tcp-addr should parse: {e}"));
        assert_eq!(cli.tcp_addr.as_deref(), Some("192.168.1.20:9443"));
    }
}

#[cfg(test)]
mod app_tests;
