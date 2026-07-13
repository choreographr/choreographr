use std::collections::HashMap;

/// Shared context window configuration used across all provider configs.
///
/// The `Default` impl initializes `context_window` to `None` and `per_model`
/// to an empty map, which is exactly what each provider config needs.
///
/// Stores a global fallback window size and a per-model map. The per-model
/// map takes priority — the first exact model-name match wins, otherwise
/// the global `context_window` is used.
#[derive(Debug, Clone, Default)]
pub struct ContextWindowConfig {
    /// Global fallback context window size for any model not in `per_model`.
    pub context_window: Option<u32>,
    /// Per-model context window overrides, keyed by exact model name.
    pub per_model: HashMap<String, u32>,
}

impl ContextWindowConfig {
    /// Resolve the context window for a specific model.
    /// Per-model entries take priority over the global fallback.
    pub fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.per_model.get(model).copied().or(self.context_window)
    }

    /// Apply optional overrides from an account config or another source.
    pub fn apply_overrides(
        &mut self,
        context_window: Option<u32>,
        per_model: Option<&HashMap<String, u32>>,
    ) {
        if let Some(cw) = context_window {
            self.context_window = Some(cw);
        }
        if let Some(map) = per_model {
            self.per_model = map.clone();
        }
    }
}
