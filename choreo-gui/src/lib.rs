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
        let server_pk = match read_server_pk(cli.server_pk.as_deref()) {
            Ok(pk) => pk,
            Err(e) => {
                eprintln!("failed to read server public key: {e}");
                std::process::exit(1);
            }
        };
        ConnectionMode::Tcp { addr, server_pk }
    } else {
        ConnectionMode::UnixSocket(socket_path())
    };

    // Store mode globally so the App component can read it.
    let _ = CONNECTION_MODE.set(mode);

    // Platform-agnostic launch facade: under the `native` feature the macro
    // cfg routes this to the Dioxus Native (Blitz) renderer, which serves both
    // desktop and Android — no desktop()/mobile() branching anywhere.
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

#[component]
fn App() -> Element {
    let (daemon_tx, mut events_rx) = use_daemon_connection();
    let mut state = use_signal(|| {
        let display_path = match CONNECTION_MODE.get() {
            Some(ConnectionMode::UnixSocket(path)) => path.clone(),
            Some(ConnectionMode::Tcp { addr, .. }) => addr.clone(),
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
}

#[cfg(test)]
mod app_tests;
