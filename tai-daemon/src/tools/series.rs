use super::{Tool, ToolRegistry, context::ToolContext, error::ToolError};
use crate::providers::types::ChatToolCall;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Weak, mpsc};
use std::thread;
use tai_keystore::ServiceCredential;

pub(crate) struct RunSeries {
    registry: Weak<ToolRegistry>,
}

impl RunSeries {
    pub fn new(registry: Weak<ToolRegistry>) -> Self {
        RunSeries { registry }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SeriesStep {
    /// Name of the tool to call.
    tool: String,
    /// Arguments for the tool. Use {{step_1}}, {{step_2}}, etc. nested in
    /// string values to reference the output of a previous step (1-based).
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct RunSeriesInput {
    /// Ordered list of tool calls to execute sequentially.
    steps: Vec<SeriesStep>,
}

impl Tool for RunSeries {
    type Args = RunSeriesInput;
    type Return = String;

    fn name(&self) -> &'static str {
        "run_series"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Execute a sequence of tool calls one at a time in order. \
         Each step runs only after the previous step succeeds. \
         If any step returns an error, the series stops immediately. \
         Use {{step_1}}, {{step_2}}, ... in step arguments to reference \
         the output of a previous step ({{step_1}} refers to the first step's output, etc.). \
         Note: placeholders found inside previous step outputs are NOT substituted \
         to avoid double-substitution — only literal {{step_N}} patterns in the \
         original arguments are resolved."
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_series(
            &self.registry,
            &args.steps,
            x_credentials,
            working_dir,
            ctx,
            None,
        )
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_series(
            &self.registry,
            &args.steps,
            x_credentials,
            working_dir,
            ctx,
            Some(output_tx),
        )
    }
}

/// Walk the JSON value tree and replace `{{step_N}}` placeholders in string
/// values with the corresponding step output (1-based index).
///
/// Uses a single-pass scan so that substituted content is never re-scanned
/// for further placeholders — preventing double-substitution when a step's
/// output happens to contain text that looks like a placeholder.
fn substitute_args(args: &Value, outputs: &HashMap<usize, String>) -> Value {
    match args {
        Value::String(s) => {
            let mut result = String::with_capacity(s.len());
            let mut rest = s.as_str();
            while let Some(start) = rest.find("{{step_") {
                // Push everything before the placeholder.
                result.push_str(&rest[..start]);
                rest = &rest[start + 7..]; // advance past "{{step_"

                // Find the closing "}}" to extract the index.
                if let Some(end) = rest.find("}}") {
                    if let Ok(idx) = rest[..end].parse::<usize>() {
                        if let Some(output) = outputs.get(&idx) {
                            result.push_str(output);
                        } else {
                            // Unknown step index — emit the placeholder as-is.
                            result.push_str(&format!("{{{{step_{idx}}}}}"));
                        }
                    } else {
                        // Non-numeric index — emit the full placeholder as-is.
                        result.push_str(&format!("{{{{step_{}}}}}", &rest[..end]));
                    }
                    rest = &rest[end + 2..]; // advance past "}}"
                } else {
                    // No closing "}}" — emit the trailing "{{step_" and stop.
                    result.push_str("{{step_");
                    result.push_str(rest);
                    rest = "";
                    break;
                }
            }
            result.push_str(rest);
            Value::String(result)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_args(v, outputs)))
                .collect(),
        ),
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| substitute_args(v, outputs)).collect())
        }
        other => other.clone(),
    }
}

/// Shared execution core for both `execute` and `execute_streaming`.
///
/// Iterates over steps sequentially. After each step checks
/// `ToolExecutionOutput.result.is_error` — on failure the series
/// stops immediately and propagates the error.
fn execute_series(
    registry: &Weak<ToolRegistry>,
    steps: &[SeriesStep],
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
    output_tx: Option<mpsc::Sender<Vec<u8>>>,
) -> Result<String, ToolError> {
    let registry = registry
        .upgrade()
        .ok_or_else(|| ToolError::Other("ToolRegistry no longer available".to_string()))?;

    if steps.is_empty() {
        return Ok("{}".to_string());
    }

    // Maps 1-based step index → raw output content from ToolResult.
    let mut step_outputs: HashMap<usize, String> = HashMap::new();

    for (i, step) in steps.iter().enumerate() {
        let step_idx = i + 1;

        // Substitute {{step_N}} placeholders with prior step outputs.
        let substituted_args = substitute_args(&step.arguments, &step_outputs);
        let args_json = serde_json::to_string(&substituted_args).map_err(|e| {
            ToolError::Other(format!(
                "failed to serialize step {step_idx} arguments: {e}"
            ))
        })?;

        let tool_call = ChatToolCall {
            id: format!("run_series/step_{step_idx}"),
            name: step.tool.clone(),
            arguments_json: args_json,
            caller: None,
        };

        // Pipe sub-tool streaming output through the relay thread to the
        // parent output channel so subscribers see output in real-time.
        let output = if let Some(parent_tx) = output_tx.as_ref() {
            let (sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>();
            let relay_handle = thread::spawn({
                let parent_tx = parent_tx.clone();
                move || {
                    for chunk in sub_rx {
                        if parent_tx.send(chunk).is_err() {
                            break;
                        }
                    }
                }
            });
            let result = registry.execute_streaming(
                &tool_call,
                sub_tx,
                x_credentials,
                working_dir,
                ctx,
                None,
            );
            // sub_tx is dropped when execute_streaming returns → relay drains.
            let _ = relay_handle.join();
            result
        } else {
            registry.execute(&tool_call, x_credentials, working_dir, ctx, None)
        };

        // Stop on first error — the series cannot proceed past a failed step.
        if output.result.is_error {
            return Err(ToolError::Other(format!(
                "step {step_idx} ('{}') failed: {}",
                step.tool, output.result.content
            )));
        }

        step_outputs.insert(step_idx, output.result.content);
    }

    serde_json::to_string(&step_outputs)
        .map_err(|e| ToolError::Other(format!("failed to serialize series results: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use std::sync::Arc;

    /// A simple echo tool used in tests.
    struct EchoTest {
        suffix: String,
    }

    impl Tool for EchoTest {
        type Args = serde_json::Value;
        type Return = String;

        fn name(&self) -> &'static str {
            "echo_test"
        }
        fn description(&self) -> &'static str {
            "echo args back"
        }
        fn execute(
            &self,
            args: Self::Args,
            _x_credentials: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            let content = serde_json::to_string(&args).unwrap_or_default();
            Ok(format!("{}{}", content, self.suffix))
        }
    }

    /// A tool that always fails.
    struct AlwaysFail;

    impl Tool for AlwaysFail {
        type Args = serde_json::Value;
        type Return = String;

        fn name(&self) -> &'static str {
            "always_fail"
        }
        fn description(&self) -> &'static str {
            "always fails"
        }
        fn execute(
            &self,
            _args: Self::Args,
            _x_credentials: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            Err(ToolError::Other("intentional failure".to_string()))
        }
    }

    fn test_registry() -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTest {
            suffix: String::new(),
        });
        reg.register(AlwaysFail);
        Arc::new(reg)
    }

    #[test]
    fn empty_steps_returns_empty_object() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput { steps: vec![] };
        let result = series.execute(input, None, None, None).unwrap();
        assert_eq!(result, "{}");
    }

    #[test]
    fn single_step_succeeds() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![SeriesStep {
                tool: "echo_test".into(),
                arguments: serde_json::json!({"msg": "hello"}),
            }],
        };
        let result = series.execute(input, None, None, None).unwrap();
        // result is a JSON-serialized HashMap — the inner value is
        // serde_json::to_string(&echo_test result) = "\"{\"msg\":\"hello\"}\"".
        let parsed: HashMap<String, String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key("1"), "expected key '1', got {parsed:?}");
    }

    #[test]
    fn multiple_steps_all_succeed() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"step": 1}),
                },
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"step": 2}),
                },
            ],
        };
        let result = series.execute(input, None, None, None).unwrap();
        let parsed: HashMap<String, String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains_key("1"));
        assert!(parsed.contains_key("2"));
    }

    #[test]
    fn stops_on_first_error() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"step": 1}),
                },
                SeriesStep {
                    tool: "always_fail".into(),
                    arguments: serde_json::json!({}),
                },
                SeriesStep {
                    // This step should never execute.
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"step": 3}),
                },
            ],
        };
        let err = series.execute(input, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("step 2"),
            "error should mention step 2, got: {err}"
        );
        assert!(
            err.to_string().contains("always_fail"),
            "error should mention tool name, got: {err}"
        );
    }

    #[test]
    fn fails_on_first_step_error() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![SeriesStep {
                tool: "always_fail".into(),
                arguments: serde_json::json!({}),
            }],
        };
        let err = series.execute(input, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("step 1"),
            "error should mention step 1, got: {err}"
        );
    }

    #[test]
    fn substitution_replaces_step_references() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        // First step echoes {"msg":"hello"}.
        // Second step uses {{step_1}} — substitution should prevent the
        // literal string "{{step_1}}" from appearing in the arguments.
        let input = RunSeriesInput {
            steps: vec![
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"msg": "hello"}),
                },
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"previous": "{{step_1}}"}),
                },
            ],
        };
        let result = series.execute(input, None, None, None).unwrap();
        let parsed: HashMap<String, String> = serde_json::from_str(&result).unwrap();

        // Both steps must have succeeded.
        assert!(parsed.contains_key("1"), "step 1 should be present");
        assert!(parsed.contains_key("2"), "step 2 should be present");

        // Step 2's output must NOT contain the literal {{step_1}} placeholder
        // — that proves substitution ran.
        let step2_output = parsed.get("2").expect("step 2 should exist");
        assert!(
            !step2_output.contains("{{step_1}}"),
            "step 2 output should not contain raw placeholder, got: {step2_output}"
        );
    }

    #[test]
    fn unknown_tool_name_fails_step() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![SeriesStep {
                tool: "nonexistent_tool".into(),
                arguments: serde_json::json!({}),
            }],
        };
        let err = series.execute(input, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("step 1"),
            "error should mention step 1, got: {err}"
        );
    }

    #[test]
    fn multi_level_substitution_chain() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"data": "first"}),
                },
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"data": "{{step_1}}", "extra": "literal"}),
                },
                SeriesStep {
                    tool: "echo_test".into(),
                    arguments: serde_json::json!({"combined": "{{step_2}}"}),
                },
            ],
        };
        let result = series.execute(input, None, None, None).unwrap();
        let parsed: HashMap<String, String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 3);
        // All three steps should have succeeded.
        for i in 1..=3 {
            assert!(
                parsed.contains_key(&i.to_string()),
                "step {i} should be present, got keys: {:?}",
                parsed.keys().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn substitution_no_match_returns_original() {
        let args = Value::String("just some text without placeholders".to_string());
        let outputs = HashMap::new();
        let result = substitute_args(&args, &outputs);
        assert_eq!(result, args);
    }

    #[test]
    fn substitution_with_nested_objects() {
        let args = serde_json::json!({
            "outer": {
                "inner": "prefix {{step_1}} suffix"
            },
            "list": ["a", "{{step_2}}", "b"]
        });
        let mut outputs = HashMap::new();
        outputs.insert(1, "result_one".to_string());
        outputs.insert(2, "result_two".to_string());

        let result = substitute_args(&args, &outputs);
        let obj = result.as_object().unwrap();
        assert_eq!(obj["outer"]["inner"], "prefix result_one suffix");
        assert_eq!(obj["list"][1], "result_two");
    }

    /// Test that the streaming path also works (smoke test).
    #[test]
    fn streaming_path_produces_same_result() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let input = RunSeriesInput {
            steps: vec![SeriesStep {
                tool: "echo_test".into(),
                arguments: serde_json::json!({"msg": "stream"}),
            }],
        };

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        // Spawn a thread to drain the output channel (avoiding buffer deadlock).
        let _drainer = thread::spawn(move || {
            for _chunk in output_rx {
                // Discard streaming output in test.
            }
        });

        let result = series
            .execute_streaming(input, None, None, output_tx, None)
            .unwrap();
        let parsed: HashMap<String, String> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key("1"));
    }

    #[test]
    fn tool_registry_gone_returns_error() {
        let reg = Arc::new(ToolRegistry::new());
        let series = RunSeries::new(Arc::downgrade(&reg));
        drop(reg); // Registry is destroyed.

        let input = RunSeriesInput {
            steps: vec![SeriesStep {
                tool: "echo_test".into(),
                arguments: serde_json::json!({}),
            }],
        };
        let err = series.execute(input, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("ToolRegistry no longer available"),
            "expected registry-gone error, got: {err}"
        );
    }

    #[test]
    fn valid_tool_schema() {
        let reg = test_registry();
        let series = RunSeries::new(Arc::downgrade(&reg));
        let schema = series.schema();
        assert!(schema.is_object());
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema should have properties");
        assert!(
            props.contains_key("steps"),
            "schema should have 'steps' property"
        );
    }
}
