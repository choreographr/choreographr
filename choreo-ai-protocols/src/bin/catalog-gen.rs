//! Regenerates `catalog/catalog.bin` from the local models.dev snapshot.
//!
//! Reads `catalog/models.dev.json` — a **local, gitignored** artifact (not
//! committed; `catalog.bin` is the only committed catalog data file) — and the
//! bundled `catalog/models-overlay.toml` policy layer from the crate directory.
//! It normalizes the snapshot into the base `ProviderEntry` catalog, verifies
//! the overlay merges cleanly over it (the shipped blob must always be
//! consistent with the bundled overlay), and writes the postcard-serialized
//! **normalized base only** to `catalog/catalog.bin` — the overlay is merged
//! at load time, never baked in.
//!
//! Snapshot sourcing: when `catalog/models.dev.json` exists locally it is used
//! as-is (offline-friendly); when it is missing the tool fetches a fresh
//! snapshot from models.dev and persists it **atomically** to that gitignored
//! path (temp file + rename, never a torn write) so later runs work offline.
//!
//! `--check` mode (`cargo run --bin catalog-gen -- --check`) serializes the
//! normalized base and compares it to the on-disk `catalog.bin` without
//! writing anything — the guard that the committed blob has not drifted from
//! the snapshot, and the CI-appropriate check (the snapshot is absent in CI,
//! so a missing local snapshot falls through to a fresh fetch there).
//!
//! Re-runnable: `cargo run --bin catalog-gen`. Normalization is deterministic
//! (providers and models keep the snapshot's JSON order), so re-running over
//! the same snapshot yields a byte-identical `catalog.bin` (verified by the
//! S3 checklist: run twice, diff).
//!
//! The binary also prints the merged provider list (`slug` + `display_name`)
//! in the exact `ProviderInfo { slug, display_name }` format `choreo-tui`'s
//! `PROVIDER_OPTIONS` table uses, so the hardcoded TUI list can be
//! regenerated from the catalog it actually ships with. The human-readable
//! summary goes through `tracing` (the fmt subscriber is initialized here) so
//! stdout stays a clean, paste-able data contract.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tracing::info;

use choreo_ai_protocols::catalog::{
    RefreshOutcome, fetch_modelsdev, merge_overlay, normalize_modelsdev, write_file_atomic,
};

fn main() -> Result<()> {
    // `fmt::init()` alone would use an EnvFilter that defaults an *unset*
    // RUST_LOG to ERROR-only (see EnvFilter::from_env), which would silently
    // swallow the status lines below. Honor RUST_LOG when set, but default to
    // INFO so `cargo run --bin catalog-gen` reports what it did.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // `--check` is detected before any write so check mode is strictly
    // read-only: even a snapshot fetch-and-cache must not mutate the tree.
    let check_only = std::env::args().any(|a| a == "--check");

    // `CARGO_MANIFEST_DIR` is set by cargo for every build (including the
    // generator run), so the paths below always resolve to the crate root.
    // Refuse to guess when it is missing rather than silently resolving the
    // data paths against whatever the CWD happens to be.
    let crate_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .context("CARGO_MANIFEST_DIR is not set; run via `cargo run --bin catalog-gen`")?,
    );
    let catalog_dir = crate_dir.join("catalog");

    let snapshot_path = catalog_dir.join("models.dev.json");
    let overlay_path = catalog_dir.join("models-overlay.toml");
    let out_path = catalog_dir.join("catalog.bin");

    // Snapshot source: the local gitignored snapshot when present (the offline
    // path), otherwise a fresh models.dev fetch persisted atomically to that
    // same path so the next run is offline.
    let snapshot = match std::fs::read_to_string(&snapshot_path) {
        Ok(src) => src,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(
                path = %snapshot_path.display(),
                "local models.dev snapshot is missing; fetching a fresh snapshot",
            );
            let outcome =
                fetch_modelsdev(None, true).context("failed to fetch the models.dev snapshot")?;
            let json = match outcome {
                RefreshOutcome::Fetched { json, .. } => json,
                // A forced fetch (no etag, no-cache) cannot legitimately 304.
                other => bail!(
                    "expected a fresh models.dev snapshot, got {other:?} \
                     (a forced no-cache fetch cannot 304)",
                ),
            };
            // Cache the fetch to the gitignored path so later runs are
            // offline. Check mode skips even this write: a torn snapshot file
            // would silently normalize to an empty base later, and check mode
            // must not mutate the tree.
            if !check_only {
                write_file_atomic(&snapshot_path, json.as_bytes()).with_context(|| {
                    format!(
                        "failed to persist the fetched snapshot to {}",
                        snapshot_path.display(),
                    )
                })?;
            }
            json
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", snapshot_path.display()));
        }
    };

    let overlay = std::fs::read_to_string(&overlay_path)
        .with_context(|| format!("failed to read {}", overlay_path.display()))?;

    let base = normalize_modelsdev(&snapshot);
    if base.is_empty() {
        bail!("models.dev snapshot produced an empty base — refusing to write an empty catalog");
    }

    // Validate the bundled overlay parses and merges; a broken overlay here
    // would silently produce a wrong merged catalog at load time, so fail
    // loudly at generation time instead.
    let merged = merge_overlay(&base, &overlay);
    if merged.is_empty() {
        bail!("overlay merge produced an empty catalog — refusing to write");
    }

    let bytes =
        postcard::to_allocvec(&base).context("failed to postcard-serialize the base catalog")?;

    if check_only {
        let embedded = std::fs::read(&out_path)
            .with_context(|| format!("failed to read {}", out_path.display()))?;
        if embedded != bytes {
            bail!("catalog.bin is stale: run `cargo run --bin catalog-gen` to regenerate");
        }
        info!("catalog.bin is up to date");
        return Ok(());
    }

    write_file_atomic(&out_path, &bytes)
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    let base_model_count: usize = base.iter().map(|e| e.models.len()).sum();
    let merged_model_count: usize = merged.iter().map(|e| e.models.len()).sum();
    info!(
        out_path = %out_path.display(),
        base_providers = base.len(),
        base_models = base_model_count,
        merged_providers = merged.len(),
        merged_models = merged_model_count,
        "wrote catalog.bin (base: {} providers / {} models; \
         merged catalog: {} providers / {} models)",
        base.len(),
        base_model_count,
        merged.len(),
        merged_model_count,
    );

    // Print the merged provider list in the TUI's `ProviderInfo` literal
    // format, so `choreo-tui/src/state.rs` `PROVIDER_OPTIONS` can be pasted
    // straight in. The list reflects the catalog the daemon actually loads
    // (base + bundled overlay), in catalog order. This stdout block is the
    // tool's output contract — keep it stable.
    println!(
        "\n// GENERATED PROVIDER_OPTIONS ({} providers):",
        merged.len()
    );
    for entry in &merged {
        println!(
            "    ProviderInfo {{\n        slug: {},\n        display_name: {},\n    }},",
            string_literal(&entry.slug),
            string_literal(&entry.display_name),
        );
    }

    Ok(())
}

/// Render a Rust string literal (escape quotes and backslashes).
fn string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
