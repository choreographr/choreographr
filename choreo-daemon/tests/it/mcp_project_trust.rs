// Only compiled with the `mcp` feature: the config loader lives behind it.
#![cfg(feature = "mcp")]

//! Integration coverage for the per-session project MCP tier: the project-root
//! walk, the trust store round-trip, and the project `.mcp.json` loader's
//! expansion gate.
//!
//! These exercise the public `choreo_daemon::mcp` API end to end (a real temp
//! filesystem, the real TOML store) but spawn no MCP server, so they are fast.
//! Marked `#[ignore = "integration"]` per AGENTS.md (they touch the
//! filesystem boundary and belong in `tests/`).

use std::path::PathBuf;

/// The project root walk finds the nearest `.mcp.json` at or below the git
/// root and never climbs above it.
#[test]
#[ignore = "integration"]
fn project_root_walk_finds_nearest_dot_mcp_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git")).expect("git dir");
    let nested = root.join("a").join("b");
    std::fs::create_dir_all(&nested).expect("nested");
    std::fs::write(root.join(".mcp.json"), "{}").expect("project file");

    assert_eq!(
        choreo_daemon::mcp::project_root_for(&nested),
        Some(root.to_path_buf()),
        "the walk must climb to the directory holding .mcp.json"
    );

    // With no `.mcp.json` anywhere there is no project root.
    std::fs::remove_file(root.join(".mcp.json")).expect("remove");
    assert_eq!(choreo_daemon::mcp::project_root_for(&nested), None);
}

/// The trust store round-trips through disk, keys on the exact canonical root
/// (no ancestor inheritance), and fails closed on a malformed file.
#[test]
#[ignore = "integration"]
fn trust_store_round_trips_and_is_exact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("proj");
    std::fs::create_dir_all(&root).expect("proj");
    let child = root.join("sub");
    std::fs::create_dir_all(&child).expect("sub");
    let path = dir.path().join("trust.toml");

    {
        let mut store = choreo_daemon::mcp::trust::McpTrustStore::load(path.clone());
        assert!(!store.is_trusted(&root), "a fresh store trusts nothing");
        store.trust(&root).expect("trust");
        assert!(store.is_trusted(&root));
        assert!(
            !store.is_trusted(&child),
            "trust must not be inherited by a child directory"
        );
    }
    // Persisted and reloaded.
    let reloaded = choreo_daemon::mcp::trust::McpTrustStore::load(path.clone());
    assert!(reloaded.is_trusted(&root));

    // A malformed file is fail-closed.
    std::fs::write(&path, "trusted = [ this is not toml").expect("garbage");
    let garbage = choreo_daemon::mcp::trust::McpTrustStore::load(path);
    assert_eq!(
        garbage.list(),
        [] as [PathBuf; 0],
        "a malformed trust file must not trust anything"
    );
}

/// The project loader reads `<root>/.mcp.json` and gates `${VAR}` expansion on
/// trust: a trusted load expands, an untrusted load preserves the literal.
#[test]
#[ignore = "integration"]
fn project_loader_gates_expansion_on_trust() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = dir.path().to_path_buf();
    std::fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"docs":{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer ${MCP_PROJECT_TEST_TOKEN}"}}}}"#,
    )
    .expect("write .mcp.json");

    let trusted = choreo_daemon::mcp::config::load_project_config(&root, true)
        .expect("load")
        .expect("present");
    assert_eq!(trusted.len(), 1);
    // The unset variable collapses to empty only under a trusted (expanding)
    // load; the literal is preserved otherwise. We assert the difference via
    // the debug transport label, since the resolved transport is an enum.
    let trusted_label = format!("{:?}", trusted[0].config.transport);
    let untrusted = choreo_daemon::mcp::config::load_project_config(&root, false)
        .expect("load")
        .expect("present");
    let untrusted_label = format!("{:?}", untrusted[0].config.transport);
    assert_ne!(
        trusted_label, untrusted_label,
        "trusted and untrusted loads must differ in whether ${{VAR}} is expanded"
    );
}
