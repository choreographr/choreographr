use super::ChatToolDefinition;

pub(crate) fn available_tools() -> Vec<ChatToolDefinition> {
    vec![
        ChatToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file from the local workspace.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to a text file"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "read_file_range",
            "Read a line range from a UTF-8 text file in the local workspace.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to a text file"
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based inclusive start line"
                    },
                    "max_lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "description": "Maximum number of lines to return"
                    }
                },
                "required": ["path", "start_line", "max_lines"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "list_files",
            "List files in a local directory.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to a directory",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "http_request",
            "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "HEAD"]
                    },
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional request headers, including Range",
                        "additionalProperties": {
                            "type": "string"
                        }
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional UTF-8 request body"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "default": 10
                    }
                },
                "required": ["method", "url"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "write_file",
            "Write a UTF-8 text file to the local workspace.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full UTF-8 file contents to write"
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "Whether to overwrite an existing file",
                        "default": true
                    },
                    "create_parents": {
                        "type": "boolean",
                        "description": "Whether to create missing parent directories",
                        "default": true
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "edit_file",
            "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file to edit"
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to replace"
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement text"
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "When true, replace all exact matches instead of requiring exactly one match",
                                    "default": false
                                }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    },
                    "expected_sha256": {
                        "type": "string",
                        "description": "Optional lowercase hex SHA-256 of the file before editing"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "When true, validate and preview the edit without writing the file",
                        "default": false
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "display_image",
            "Display a PNG, JPEG, or SVG image in the client UI.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "mime_type": {
                        "type": "string",
                        "enum": ["image/png", "image/jpeg", "image/svg+xml"]
                    },
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to an image file"
                    },
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL to an image"
                    },
                    "base64_data": {
                        "type": "string",
                        "description": "Raw image bytes encoded as base64"
                    },
                    "svg_text": {
                        "type": "string",
                        "description": "Inline SVG document text; only valid with mime_type image/svg+xml"
                    },
                    "alt": {
                        "type": "string",
                        "description": "Optional accessible description"
                    }
                },
                "required": ["mime_type"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_status",
            "Show the status of the Git repository containing the given path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_diff_unstaged",
            "Show the unstaged diff for a file or repository.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_diff_staged",
            "Show the staged diff for a file or repository.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_log",
            "Show recent Git commits for the repository containing the given path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "max_count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 10
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_show",
            "Show a Git object or commit.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "object": {
                        "type": "string",
                        "description": "Commit, tag, or other Git object expression"
                    }
                },
                "required": ["object"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_branch",
            "List branches in the Git repository containing the given path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_checkout",
            "Check out a Git branch or commit.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "target": {
                        "type": "string",
                        "description": "Branch, tag, or commit to check out"
                    }
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_checkout_new_branch",
            "Create and check out a new Git branch.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "branch": {
                        "type": "string",
                        "description": "New branch name"
                    },
                    "start_point": {
                        "type": "string",
                        "description": "Optional start point, defaults to HEAD"
                    }
                },
                "required": ["branch"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_commit",
            "Create a Git commit from the current index.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "message": {
                        "type": "string",
                        "description": "Commit message"
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_add",
            "Stage a file or pathspec in Git.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "pathspec": {
                        "type": "string",
                        "description": "File or pathspec to stage"
                    }
                },
                "required": ["pathspec"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_reset_path",
            "Unstage a file or pathspec in Git.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "pathspec": {
                        "type": "string",
                        "description": "File or pathspec to unstage"
                    }
                },
                "required": ["pathspec"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_diff_between",
            "Show the diff between two Git revisions.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "base": {
                        "type": "string",
                        "description": "Base revision"
                    },
                    "head": {
                        "type": "string",
                        "description": "Head revision"
                    }
                },
                "required": ["base", "head"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_merge_base",
            "Compute the merge base between two Git revisions.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "left": {
                        "type": "string",
                        "description": "Left revision"
                    },
                    "right": {
                        "type": "string",
                        "description": "Right revision"
                    }
                },
                "required": ["left", "right"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_fetch",
            "Fetch from a Git remote.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "remote": {
                        "type": "string",
                        "description": "Remote name",
                        "default": "origin"
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_pull",
            "Pull from a Git remote branch.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "remote": {
                        "type": "string",
                        "description": "Remote name",
                        "default": "origin"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Remote branch name"
                    }
                },
                "required": ["branch"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_push",
            "Push to a Git remote branch.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "remote": {
                        "type": "string",
                        "description": "Remote name",
                        "default": "origin"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Remote branch name"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Force push",
                        "default": false
                    }
                },
                "required": ["branch"],
                "additionalProperties": false
            }),
        ),
    ]
}
