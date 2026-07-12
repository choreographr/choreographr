use super::ToolError;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct RandomArgs {
    /// Kind of random value: "int" (default), "float", "bool", "bytes", "uuid"
    pub r#type: Option<String>,
    /// Minimum value for type "int" (inclusive). Default: 0
    pub min: Option<i64>,
    /// Maximum value for type "int" (inclusive). Default: 100
    pub max: Option<i64>,
    /// Minimum value for type "float" (inclusive). Default: 0.0
    pub min_float: Option<f64>,
    /// Maximum value for type "float" (exclusive). Default: 1.0
    pub max_float: Option<f64>,
    /// Number of random bytes for type "bytes". Default: 16, Max: 65536
    pub length: Option<u32>,
    /// Optional seed for deterministic (reproducible) output
    pub seed: Option<u64>,
}

pub(crate) fn execute_random_tool(
    args: &RandomArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolError> {
    match args.seed {
        Some(s) => generate(&mut StdRng::seed_from_u64(s), args),
        None => generate(&mut rand::rng(), args),
    }
}

fn generate(rng: &mut impl RngExt, args: &RandomArgs) -> Result<String, ToolError> {
    let type_ = args.r#type.as_deref().unwrap_or("int");

    match type_ {
        "int" => {
            let min = args.min.unwrap_or(0);
            let max = args.max.unwrap_or(100);
            if min > max {
                return Err(ToolError::Other(
                    "min must not be greater than max".to_string(),
                ));
            }
            let value = rng.random_range(min..=max);
            Ok(format!("{value}"))
        }

        "float" => {
            let min = args.min_float.unwrap_or(0.0);
            let max = args.max_float.unwrap_or(1.0);
            if min >= max {
                return Err(ToolError::Other(
                    "min_float must be less than max_float".to_string(),
                ));
            }
            let value: f64 = rng.random_range(min..max);
            Ok(format!("{value}"))
        }

        "bool" => {
            let value: bool = rng.random();
            Ok(value.to_string())
        }

        "bytes" => {
            let length = args.length.unwrap_or(16).min(65536) as usize;
            let mut buf = vec![0u8; length];
            rng.fill_bytes(&mut buf);
            let encoded = BASE64.encode(&buf);
            Ok(encoded)
        }

        "uuid" => {
            let mut bytes = [0u8; 16];
            rng.fill_bytes(&mut bytes);
            // Set version nibble to 0100 (v4)
            bytes[6] = (bytes[6] & 0x0f) | 0x40;
            // Set variant to 10xx (RFC 9562)
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            let uuid = format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                bytes[0],
                bytes[1],
                bytes[2],
                bytes[3],
                bytes[4],
                bytes[5],
                bytes[6],
                bytes[7],
                bytes[8],
                bytes[9],
                bytes[10],
                bytes[11],
                bytes[12],
                bytes[13],
                bytes[14],
                bytes[15],
            );
            Ok(uuid)
        }

        other => Err(ToolError::Other(format!(
            "unknown random type: {other}; expected one of: int, float, bool, bytes, uuid"
        ))),
    }
}

pub(crate) struct Random;

define_tool!(
    Random,
    "random",
    "Generate random values: integers, floats, booleans, bytes (base64), or UUID v4. Supports optional seed for reproducibility.",
    RandomArgs,
    execute_random_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "type": {
                "type": "string",
                "enum": ["int", "float", "bool", "bytes", "uuid"],
                "description": "Kind of random value to generate (default: int)"
            },
            "min": {
                "type": "integer",
                "description": "Minimum value for type 'int' (inclusive, default: 0)"
            },
            "max": {
                "type": "integer",
                "description": "Maximum value for type 'int' (inclusive, default: 100)"
            },
            "min_float": {
                "type": "number",
                "description": "Minimum value for type 'float' (inclusive, default: 0.0)"
            },
            "max_float": {
                "type": "number",
                "description": "Maximum value for type 'float' (exclusive, default: 1.0)"
            },
            "length": {
                "type": "integer",
                "minimum": 1,
                "maximum": 65536,
                "description": "Number of random bytes for type 'bytes' (default: 16, max: 65536)"
            },
            "seed": {
                "type": "integer",
                "description": "Optional seed for deterministic (reproducible) results"
            }
        },
        "additionalProperties": false
    }),
    "core"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_int_default_range() {
        let args = RandomArgs {
            r#type: None,
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        let value: i64 = result.parse().unwrap();
        assert!((0..=100).contains(&value));
    }

    #[test]
    fn random_int_range_respected() {
        let args = RandomArgs {
            r#type: Some("int".into()),
            min: Some(50),
            max: Some(60),
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        let value: i64 = result.parse().unwrap();
        assert!((50..=60).contains(&value));
    }

    #[test]
    fn random_int_min_greater_than_max_error() {
        let args = RandomArgs {
            r#type: Some("int".into()),
            min: Some(100),
            max: Some(0),
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None);
        assert!(result.is_err());
    }

    #[test]
    fn random_float_default_range() {
        let args = RandomArgs {
            r#type: Some("float".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        let value: f64 = result.parse().unwrap();
        assert!((0.0..1.0).contains(&value));
    }

    #[test]
    fn random_float_range_respected() {
        let args = RandomArgs {
            r#type: Some("float".into()),
            min: None,
            max: None,
            min_float: Some(5.0),
            max_float: Some(10.0),
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        let value: f64 = result.parse().unwrap();
        assert!((5.0..10.0).contains(&value));
    }

    #[test]
    fn random_bool() {
        let args = RandomArgs {
            r#type: Some("bool".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        let value: bool = result.parse().unwrap();
        // Just verify it's a valid bool — no assertion on which value
        assert!(value || !value);
    }

    #[test]
    fn random_bytes_default_length() {
        let args = RandomArgs {
            r#type: Some("bytes".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        // 16 bytes base64-encoded => 24 chars (no padding)
        assert_eq!(result.len(), 24);
    }

    #[test]
    fn random_bytes_custom_length() {
        let args = RandomArgs {
            r#type: Some("bytes".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: Some(32),
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        // 32 bytes base64-encoded => ceil(32*4/3) => 44 chars
        assert_eq!(result.len(), 44);
    }

    #[test]
    fn random_uuid_format() {
        let args = RandomArgs {
            r#type: Some("uuid".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None).unwrap();
        // UUID v4 format: 8-4-4-4-12 hex digits
        assert_eq!(result.len(), 36);
        // Check version nibble at position 14 (0-indexed)
        assert_eq!(&result[14..15], "4");
        // Check variant at position 19
        assert!(["8", "9", "a", "b"].contains(&&result[19..20]));
    }

    #[test]
    fn random_unknown_type_error() {
        let args = RandomArgs {
            r#type: Some("invalid".into()),
            min: None,
            max: None,
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None);
        assert!(result.is_err());
    }

    #[test]
    fn random_seed_deterministic() {
        let args = RandomArgs {
            r#type: Some("int".into()),
            min: Some(0),
            max: Some(1000),
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(12345),
        };
        let a = execute_random_tool(&args, None).unwrap();
        let b = execute_random_tool(&args, None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn random_different_seeds_produce_different_values() {
        let args_a = RandomArgs {
            r#type: Some("int".into()),
            min: Some(0),
            max: Some(1000000),
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(1),
        };
        let args_b = RandomArgs {
            r#type: Some("int".into()),
            min: Some(0),
            max: Some(1000000),
            min_float: None,
            max_float: None,
            length: None,
            seed: Some(2),
        };
        let a = execute_random_tool(&args_a, None).unwrap();
        let b = execute_random_tool(&args_b, None).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn random_float_min_gte_max_error() {
        let args = RandomArgs {
            r#type: Some("float".into()),
            min: None,
            max: None,
            min_float: Some(10.0),
            max_float: Some(5.0),
            length: None,
            seed: Some(42),
        };
        let result = execute_random_tool(&args, None);
        assert!(result.is_err());
    }
}
