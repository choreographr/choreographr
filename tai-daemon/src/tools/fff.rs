use super::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use fff_search::*;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

const FFF_SCAN_TIMEOUT_SECS: u64 = 60;
const FFF_DEFAULT_MAX_RESULTS: usize = 50;
const FFF_MAX_RESULTS_CAP: usize = 100;

#[derive(Debug, Deserialize)]
struct FffArgs {
    query: String,
    path: Option<String>,
    mode: Option<String>,
    pattern_type: Option<String>,
    max_results: Option<usize>,
}

struct FffState {
    shared_picker: SharedFilePicker,
    _shared_frecency: SharedFrecency,
}

fn frecency_db_path(path_hash: &str) -> PathBuf {
    let data_dir = dirs::data_dir().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".local/share")
    });
    data_dir.join("fff").join(path_hash).join("frecency")
}

fn init_new_state(abs_path: &Path) -> std::result::Result<FffState, ToolError> {
    let path_str = abs_path
        .to_str()
        .ok_or_else(|| ToolError::Other("non-utf8 path".to_string()))?;
    let hash = super::sha256_hex(path_str);
    let frecency_path = frecency_db_path(&hash);

    std::fs::create_dir_all(&frecency_path).map_err(|e| {
        ToolError::Other(format!(
            "create frecency dir {}: {e}",
            frecency_path.display()
        ))
    })?;

    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    let frecency = FrecencyTracker::open(&frecency_path)
        .map_err(|e| ToolError::Other(format!("open frecency db: {e}")))?;

    shared_frecency
        .init(frecency)
        .map_err(|e| ToolError::Other(format!("init frecency: {e}")))?;

    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        FilePickerOptions {
            base_path: path_str.to_string(),
            mode: FFFMode::Ai,
            watch: false,
            ..Default::default()
        },
    )
    .map_err(|e| ToolError::Other(format!("create file picker: {e}")))?;

    shared_picker.wait_for_scan(Duration::from_secs(FFF_SCAN_TIMEOUT_SECS));

    Ok(FffState {
        shared_picker,
        _shared_frecency: shared_frecency,
    })
}

fn get_or_init_state(path: &str) -> std::result::Result<Arc<FffState>, ToolError> {
    static CACHE: OnceLock<RwLock<HashMap<PathBuf, Arc<FffState>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));

    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));

    {
        let guard = cache
            .read()
            .map_err(|e| ToolError::Other(format!("cache lock: {e}")))?;
        if let Some(state) = guard.get(&abs) {
            return Ok(state.clone());
        }
    }

    let state = Arc::new(init_new_state(&abs)?);
    cache
        .write()
        .map_err(|e| ToolError::Other(format!("cache lock: {e}")))?
        .insert(abs, state.clone());
    Ok(state)
}

define_tool!(
    Fff,
    "fff",
    "Search file contents or file names using fff. Supports grep (content search) and files (file name search) modes.",
    execute_fff_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query. Supports advanced syntax like 'ext:rs my_function' or 'path:src/**'. For file name search, this is a fuzzy pattern."
            },
            "mode": {
                "type": "string",
                "enum": ["grep", "files"],
                "description": "Search mode: 'grep' for content search (default), 'files' for file name fuzzy search",
                "default": "grep"
            },
            "path": {
                "type": "string",
                "description": "Root path for the search (default: current directory)"
            },
            "pattern_type": {
                "type": "string",
                "enum": ["plain", "regex", "fuzzy"],
                "description": "Pattern matching mode for grep: 'plain' (default), 'regex', or 'fuzzy'",
                "default": "plain"
            },
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 50
            }
        },
        "required": ["query"],
        "additionalProperties": false
    }),
    "core"
);

pub(crate) fn execute_fff_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_fff_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_fff_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> std::result::Result<String, ToolError> {
    let args: FffArgs = serde_json::from_str(arguments_json)?;

    let path = args.path.as_deref().unwrap_or(".");
    let resolved = super::resolve_path(path, cwd);
    let state = get_or_init_state(&resolved.display().to_string())?;

    let guard = state
        .shared_picker
        .read()
        .map_err(|e| ToolError::Other(format!("picker lock error: {e}")))?;
    let picker = guard.as_ref().ok_or_else(|| {
        ToolError::Other(
            "fff picker not yet initialized (scan may still be in progress)".to_string(),
        )
    })?;

    let mode = args.mode.as_deref().unwrap_or("grep");
    let max_results = args
        .max_results
        .unwrap_or(FFF_DEFAULT_MAX_RESULTS)
        .min(FFF_MAX_RESULTS_CAP);

    match mode {
        "files" => {
            let parser = QueryParser::new(FileSearchConfig);
            let query = parser.parse(&args.query);

            let result = picker.fuzzy_search(
                &query,
                None,
                FuzzySearchOptions {
                    pagination: PaginationArgs {
                        offset: 0,
                        limit: max_results,
                    },
                    ..Default::default()
                },
            );

            if result.items.is_empty() {
                return Ok(String::new());
            }

            let mut lines: Vec<String> = Vec::with_capacity(result.items.len());
            for item in &result.items {
                lines.push(item.relative_path(picker));
            }

            Ok(truncate_tool_output(&lines.join("\n")))
        }
        _ => {
            let parser = QueryParser::new(AiGrepConfig);
            let query = parser.parse(&args.query);

            let pattern_type = args.pattern_type.as_deref().unwrap_or("plain");
            let grep_mode = match pattern_type {
                "regex" => GrepMode::Regex,
                "fuzzy" => GrepMode::Fuzzy,
                _ => GrepMode::PlainText,
            };

            let result = picker.grep(
                &query,
                &GrepSearchOptions {
                    page_limit: max_results,
                    mode: grep_mode,
                    trim_whitespace: true,
                    ..Default::default()
                },
            );

            if result.matches.is_empty() {
                return Ok(String::new());
            }

            let mut lines: Vec<String> = Vec::with_capacity(result.matches.len());
            for m in &result.matches {
                if let Some(file_item) = result.files.get(m.file_index) {
                    lines.push(format!(
                        "{}:{}:{}",
                        file_item.relative_path(picker),
                        m.line_number,
                        m.line_content
                    ));
                }
            }

            Ok(truncate_tool_output(&lines.join("\n")))
        }
    }
}
