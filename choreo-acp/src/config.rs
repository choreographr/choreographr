use crate::acp_jsonrpc::{ConfigOption, ConfigOptionType, ConfigOptionValue, SelectOption};

/// Build a `"model"` config option from the daemon's model list.
///
/// The option type is `Select` with one entry per model.  If a model is
/// currently selected, its value is the `current_value`; otherwise the
/// current value is an empty string.
pub fn build_model_config(models: &[String], selected: &Option<String>) -> ConfigOption {
    let options: Vec<SelectOption> = models
        .iter()
        .map(|m| SelectOption {
            value: m.clone(),
            name: Some(m.clone()),
        })
        .collect();

    let current_value = match selected {
        Some(model) => ConfigOptionValue::String(model.clone()),
        None => ConfigOptionValue::String(String::new()),
    };

    ConfigOption {
        id: "model".into(),
        name: "Model".into(),
        description: Some("The AI model to use for generating responses".into()),
        category: Some("general".into()),
        option_type: ConfigOptionType::Select,
        current_value,
        options: Some(options),
    }
}

/// Build a `"reasoning_effort"` config option.
///
/// The option type is `Select` with the standard off/low/medium/high
/// choices.  Defaults to `"medium"` when no effort is set.
pub fn build_reasoning_effort_config(current: Option<String>) -> ConfigOption {
    let current_value = match current {
        Some(effort) => ConfigOptionValue::String(effort),
        None => ConfigOptionValue::String("medium".into()),
    };

    let options = vec![
        SelectOption {
            value: "off".into(),
            name: Some("Off (no reasoning)".into()),
        },
        SelectOption {
            value: "low".into(),
            name: Some("Low".into()),
        },
        SelectOption {
            value: "medium".into(),
            name: Some("Medium".into()),
        },
        SelectOption {
            value: "high".into(),
            name: Some("High".into()),
        },
    ];

    ConfigOption {
        id: "reasoning_effort".into(),
        name: "Reasoning Effort".into(),
        description: Some("Controls how much reasoning the model performs before answering".into()),
        category: Some("general".into()),
        option_type: ConfigOptionType::Select,
        current_value,
        options: Some(options),
    }
}

/// Build a `"tool_groups"` config option.
///
/// Currently hardcoded — future versions may query the daemon for
/// available tool groups.
pub fn build_tool_groups_config() -> ConfigOption {
    ConfigOption {
        id: "tool_groups".into(),
        name: "Tool Groups".into(),
        description: Some("Which tool groups are enabled for the session".into()),
        category: Some("general".into()),
        option_type: ConfigOptionType::TextField,
        current_value: ConfigOptionValue::String("read,edit,terminal".into()),
        options: None,
    }
}

/// Build all config options from current daemon state.
///
/// Returns the canonical set of `ConfigOption` objects that choreo-acp
/// advertises in the `InitializeResult` and `NewSessionResult`.
pub fn build_config_options(
    models: &[String],
    selected_model: &Option<String>,
    reasoning_effort: Option<String>,
) -> Vec<ConfigOption> {
    vec![
        build_model_config(models, selected_model),
        build_reasoning_effort_config(reasoning_effort),
        build_tool_groups_config(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_config_contains_all_models() {
        let models = vec!["claude-4".into(), "gpt-5".into()];
        let config = build_model_config(&models, &Some("claude-4".into()));

        assert_eq!(config.id, "model");
        assert!(matches!(config.option_type, ConfigOptionType::Select));
        let opts = config.options.unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].value, "claude-4");
        assert_eq!(opts[1].value, "gpt-5");
        match config.current_value {
            ConfigOptionValue::String(v) => assert_eq!(v, "claude-4"),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn model_config_empty_selection() {
        let models = vec!["claude-4".into()];
        let config = build_model_config(&models, &None);
        match config.current_value {
            ConfigOptionValue::String(v) => assert!(v.is_empty()),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn reasoning_effort_config_has_all_choices() {
        let config = build_reasoning_effort_config(Some("high".into()));
        assert_eq!(config.id, "reasoning_effort");
        let opts = config.options.unwrap();
        assert_eq!(opts.len(), 4);
        match config.current_value {
            ConfigOptionValue::String(v) => assert_eq!(v, "high"),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn reasoning_effort_defaults_to_medium() {
        let config = build_reasoning_effort_config(None);
        match config.current_value {
            ConfigOptionValue::String(v) => assert_eq!(v, "medium"),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn tool_groups_config_hardcoded() {
        let config = build_tool_groups_config();
        assert_eq!(config.id, "tool_groups");
        assert!(matches!(config.option_type, ConfigOptionType::TextField));
        match config.current_value {
            ConfigOptionValue::String(v) => assert_eq!(v, "read,edit,terminal"),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn build_config_options_returns_three_options() {
        let models = vec!["claude-4".into()];
        let opts = build_config_options(&models, &Some("claude-4".into()), None);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].id, "model");
        assert_eq!(opts[1].id, "reasoning_effort");
        assert_eq!(opts[2].id, "tool_groups");
    }
}
