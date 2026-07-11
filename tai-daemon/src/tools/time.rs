use super::ToolError;
use std::path::Path;
use std::time::SystemTime;

/// Returns the current Unix timestamp in milliseconds since the epoch.
///
/// Propagates a [`ToolError`] if the system clock is set before UNIX_EPOCH
/// (essentially impossible on real hardware, but handled gracefully rather
/// than silently returning 0).
pub(crate) fn execute_get_current_time(_args: &(), _cwd: Option<&Path>) -> Result<u64, ToolError> {
    let millis = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ToolError::Other(format!("system clock before epoch: {e}")))?
        .as_millis() as u64;
    tracing::debug!(millis, "get_current_time");
    Ok(millis)
}

pub(crate) struct GetCurrentTime;

define_tool!(
    GetCurrentTime,
    "get_current_time",
    "Get the current Unix timestamp in milliseconds since epoch",
    (),
    u64,
    execute_get_current_time,
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
    "core"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_returns_reasonable_millis() {
        let result = execute_get_current_time(&(), None).unwrap();
        // Should be around 1.7+ trillion ms (year 2024+), well short of u64::MAX
        assert!(
            result > 1_700_000_000_000,
            "timestamp should be >= year 2024, got {result}"
        );
        assert!(
            result < 2_000_000_000_000,
            "timestamp should be before year 2033, got {result}"
        );
    }
}
