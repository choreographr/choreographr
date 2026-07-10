use super::*;
use crate::tools::fs::{
    EditFileArgs, LineCountArgs, ListFilesArgs, ReadFileArgs, ReadFileRangeArgs, TextEditArgs,
    WriteFileArgs,
};
use crate::tools::http::HttpRequestArgs;
use std::io::{Read, Write};
use std::net::TcpListener;

fn spawn_http_tool_server() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http tool server");
    let addr = listener.local_addr().expect("http tool server addr");
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            std::thread::spawn(move || {
                let mut buffer = vec![0_u8; 32 * 1024];
                let Ok(read_len) = stream.read(&mut buffer) else {
                    return;
                };
                if read_len == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read_len]).to_string();
                let first_line = request.lines().next().unwrap_or_default().to_string();

                if first_line.starts_with("HEAD /meta ") {
                    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 42\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /range ") {
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                    let body = "abcdefghij";
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-type: text/plain\r\ncontent-length: {}\r\ncontent-range: bytes 0-9/100\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    assert_eq!(range, "bytes=0-9");
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /binary ") {
                    let body = [0_u8, 159, 146, 150];
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("GET /long ") {
                    let body = "x".repeat((16 * 1024) + 128);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                if first_line.starts_with("POST /echo ") {
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body)
                        .unwrap_or_default();
                    let response_body = format!("echo:{body}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                let body = "not found";
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (format!("http://{addr}"), handle)
}

fn test_temp_path(prefix: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}.txt"))
}

#[test]
fn read_file_range_tool_reads_numbered_line_chunks() {
    let path = test_temp_path("tai-read-range-tool");
    std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").expect("seed file");

    let result = execute_read_file_range_tool(
        &ReadFileRangeArgs {
            path: path.display().to_string(),
            start_line: 2,
            max_lines: 2,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("lines: 2-3 of 4"), "{}", content);
    assert!(content.contains("2 | beta"), "{}", content);
    assert!(content.contains("3 | gamma"), "{}", content);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_file_range_tool_clamps_to_eof() {
    let path = test_temp_path("tai-read-range-eof-tool");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("seed file");

    let result = execute_read_file_range_tool(
        &ReadFileRangeArgs {
            path: path.display().to_string(),
            start_line: 2,
            max_lines: 10,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("lines: 2-3 of 3"), "{}", content);
    assert!(content.contains("2 | beta"), "{}", content);
    assert!(content.contains("3 | gamma"), "{}", content);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_file_range_tool_rejects_start_line_past_eof() {
    let path = test_temp_path("tai-read-range-past-eof-tool");
    std::fs::write(&path, "alpha\nbeta\n").expect("seed file");

    let result = execute_read_file_range_tool(
        &ReadFileRangeArgs {
            path: path.display().to_string(),
            start_line: 5,
            max_lines: 1,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("past end of file"), "{}", err);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_file_range_tool_rejects_excessive_max_lines() {
    let path = test_temp_path("tai-read-range-max-lines-tool");
    std::fs::write(&path, "alpha\n").expect("seed file");

    let result = execute_read_file_range_tool(
        &ReadFileRangeArgs {
            path: path.display().to_string(),
            start_line: 1,
            max_lines: 201,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("max_lines must be <= 200"), "{}", err);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_file_tool_writes_new_file() {
    let path = test_temp_path("tai-write-tool");

    execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "hello from write tool\n".into(),
            overwrite: Some(true),
            create_parents: Some(true),
        },
        None,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "hello from write tool\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_file_tool_refuses_overwrite_when_disabled() {
    let path = test_temp_path("tai-write-tool-existing");
    std::fs::write(&path, "original\n").expect("seed file");

    let result = execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "replacement\n".into(),
            overwrite: Some(false),
            create_parents: Some(true),
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("refusing to overwrite existing file"),
        "{}",
        err
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "original\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_file_tool_creates_parent_directories() {
    let dir = test_temp_path("tai-write-tool-dir").with_extension("");
    let path = dir.join("nested/output.txt");

    execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "nested hello\n".into(),
            overwrite: Some(true),
            create_parents: Some(true),
        },
        None,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "nested hello\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_file_tool_replaces_single_unique_match() {
    let path = test_temp_path("tai-edit-tool-single");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "beta".into(),
                new_text: "delta".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("edited file:"), "{}", content);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "alpha\ndelta\ngamma\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_fails_when_old_text_is_missing() {
    let path = test_temp_path("tai-edit-tool-missing");
    std::fs::write(&path, "hello\nworld\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "absent".into(),
                new_text: "present".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("old_text not found"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "hello\nworld\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_fails_on_ambiguous_non_replace_all_edit() {
    let path = test_temp_path("tai-edit-tool-ambiguous");
    std::fs::write(&path, "repeat\nrepeat\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "repeat".into(),
                new_text: "done".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("matched 2 locations"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "repeat\nrepeat\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_supports_replace_all_and_dry_run() {
    let path = test_temp_path("tai-edit-tool-replace-all");
    std::fs::write(&path, "foo\nfoo\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: Some(true),
            }],
            expected_sha256: None,
            dry_run: Some(true),
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("would edit file:"), "{}", content);
    assert!(content.contains("2 replacements"), "{}", content);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "foo\nfoo\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_validates_expected_sha256() {
    let path = test_temp_path("tai-edit-tool-sha");
    let original = "red\nblue\n";
    std::fs::write(&path, original).expect("seed file");
    let expected_sha256 = sha256_hex(original);

    let success = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "blue".into(),
                new_text: "green".into(),
                replace_all: None,
            }],
            expected_sha256: Some(expected_sha256.clone()),
            dry_run: None,
        },
        None,
    );
    assert!(success.is_ok(), "{}", success.unwrap_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "red\ngreen\n"
    );

    let failure = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "green".into(),
                new_text: "purple".into(),
                replace_all: None,
            }],
            expected_sha256: Some(expected_sha256),
            dry_run: None,
        },
        None,
    );
    assert!(failure.is_err(), "{}", failure.unwrap_err());
    let err = failure.unwrap_err().to_string();
    assert!(err.contains("expected_sha256 mismatch"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "red\ngreen\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn http_request_tool_supports_range_header() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/range"),
            headers: [("Range".into(), "bytes=0-9".into())].into(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(
        content.contains("status: 206 Partial Content"),
        "{}",
        content
    );
    assert!(
        content.contains("content-range: bytes 0-9/100"),
        "{}",
        content
    );
    assert!(content.ends_with("abcdefghij"), "{}", content);

    drop(server);
}

#[test]
fn http_request_tool_supports_head_requests() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "HEAD".into(),
            url: format!("{base_url}/meta"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("status: 200 OK"), "{}", content);
    assert!(content.contains("accept-ranges: bytes"), "{}", content);
    assert!(content.ends_with("\n\n"), "{}", content);

    drop(server);
}

#[test]
fn http_request_tool_summarizes_non_text_responses() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/binary"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(
        content.contains("content-type: application/octet-stream"),
        "{}",
        content
    );
    assert!(
        content.ends_with("body omitted: non-text response"),
        "{}",
        content
    );

    drop(server);
}

#[test]
fn http_request_tool_truncates_large_text_responses() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "GET".into(),
            url: format!("{base_url}/long"),
            headers: Default::default(),
            body: None,
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("...[truncated]"), "{}", content);

    drop(server);
}

#[test]
fn http_request_tool_supports_post_body() {
    let (base_url, server) = spawn_http_tool_server();
    let result = execute_http_request_tool(
        &HttpRequestArgs {
            method: "POST".into(),
            url: format!("{base_url}/echo"),
            headers: Default::default(),
            body: Some("hello".into()),
            timeout_secs: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.ends_with("echo:hello"), "{}", content);

    drop(server);
}
