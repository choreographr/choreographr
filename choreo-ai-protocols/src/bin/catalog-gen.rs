//! Regenerates `catalog/catalog.bin` from the checked-in models.dev snapshot.
//!
//! Reads `catalog/models.dev.json` (the pinned models.dev snapshot) and
//! `catalog/models-overlay.toml` (the bundled policy layer) from the crate
//! directory, normalizes the snapshot into the base `ProviderEntry` catalog,
//! verifies the overlay merges cleanly over it (the shipped blob must always
//! be consistent with the bundled overlay), and writes the postcard-serialized
//! **normalized base only** to `catalog/catalog.bin` — the overlay is merged at
//! load time, never baked in.
//!
//! Re-runnable: `cargo run --bin catalog-gen`. Normalization is deterministic
//! (providers and models keep the snapshot's JSON order), so re-running over
//! the same snapshot yields a byte-identical `catalog.bin` (verified by the
//! S3 checklist: run twice, diff).
//!
//! The binary also prints the merged provider list (`slug` + `display_name`)
//! in the exact `ProviderInfo { slug, display_name }` format `choreo-tui`'s
//! `PROVIDER_OPTIONS` table uses, so the hardcoded TUI list can be
//! regenerated from the catalog it actually ships with.

use std::path::PathBuf;

fn main() {
    // `CARGO_MANIFEST_DIR` is set by cargo for every build (including the
    // generator run), so the paths below always resolve to the crate root.
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let catalog_dir = crate_dir.join("catalog");

    let snapshot_path = catalog_dir.join("models.dev.json");
    let overlay_path = catalog_dir.join("models-overlay.toml");
    let out_path = catalog_dir.join("catalog.bin");

    let snapshot = match std::fs::read_to_string(&snapshot_path) {
        Ok(src) => src,
        Err(e) => fatal(&format!("failed to read {}: {e}", snapshot_path.display())),
    };
    let overlay = match std::fs::read_to_string(&overlay_path) {
        Ok(src) => src,
        Err(e) => fatal(&format!("failed to read {}: {e}", overlay_path.display())),
    };

    let base = choreo_ai_protocols::catalog::normalize_modelsdev(&snapshot);
    if base.is_empty() {
        fatal("models.dev snapshot produced an empty base — refusing to write an empty catalog");
    }

    // Validate the bundled overlay parses and merges; a broken overlay here
    // would silently produce a wrong merged catalog at load time, so fail
    // loudly at generation time instead.
    let merged = choreo_ai_protocols::catalog::merge_overlay(&base, &overlay);
    if merged.is_empty() {
        fatal("overlay merge produced an empty catalog — refusing to write");
    }

    let bytes = match postcard::to_allocvec(&base) {
        Ok(bytes) => bytes,
        Err(e) => fatal(&format!(
            "failed to postcard-serialize the base catalog: {e}"
        )),
    };
    if let Err(e) = std::fs::write(&out_path, &bytes) {
        fatal(&format!("failed to write {}: {e}", out_path.display()));
    }

    let provider_count = merged.len();
    let model_count: usize = merged.iter().map(|e| e.models.len()).sum();
    println!(
        "wrote {} ({} providers, {} models in base; merged catalog: {} providers, {} models)",
        out_path.display(),
        base.len(),
        base.iter().map(|e| e.models.len()).sum::<usize>(),
        provider_count,
        model_count,
    );

    // Print the merged provider list in the TUI's `ProviderInfo` literal
    // format, so `choreo-tui/src/state.rs` `PROVIDER_OPTIONS` can be pasted
    // straight in. The list reflects the catalog the daemon actually loads
    // (base + bundled overlay), in catalog order.
    println!(
        "\n// GENERATED PROVIDER_OPTIONS ({} providers):",
        provider_count
    );
    for entry in &merged {
        println!(
            "    ProviderInfo {{\n        slug: {},\n        display_name: {},\n    }},",
            string_literal(&entry.slug),
            string_literal(&entry.display_name),
        );
    }
}

/// Print a fatal error to stderr and exit non-zero (a generator failure must
/// be loud — it produces the artifact every binary embeds).
fn fatal(message: &str) -> ! {
    eprintln!("catalog-gen: {message}");
    std::process::exit(1);
}

/// Render a Rust string literal (escape quotes and backslashes).
fn string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
