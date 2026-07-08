use std::path::Path;
use tai_daemon::execute_run_riscv_tool;

#[test]
#[ignore]
fn simple_write_no_alloc() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { tai::write(b\"Hello from VM!\"); }", "allocator": false}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(
        result.content.contains("Hello from VM!"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn simple_write_with_alloc() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { tai::write(b\"Hello from VM!\"); }"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(
        result.content.contains("Hello from VM!"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn exit_zero() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { tai::exit(0); }"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    // The tool prepends the formatted source as a markdown code block.
    assert!(
        result.content.contains("```rust"),
        "expected source block: {}",
        result.content
    );
    assert!(
        result.content.contains("fn main()"),
        "expected source: {}",
        result.content
    );
}

#[test]
#[ignore]
fn with_args() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { let args = tai::args(); if args.len() > 0 { tai::write(&args[0]); } }", "args": ["hello"]}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("hello"), "{}", result.content);
}

#[test]
#[ignore]
fn cycle_limit_enforced() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { loop {} }", "max_cycles": 100}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("cycle limit") || result.content.contains("VM error"),
        "expected cycle limit or VM error: {}",
        result.content
    );
}

#[test]
#[ignore]
fn compilation_error_invalid_rust() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { this is bad rust }"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("compilation error") || result.content.contains("compile"),
        "expected compilation error: {}",
        result.content
    );
}

#[test]
#[ignore]
fn invalid_json() {
    let result = execute_run_riscv_tool(r#"not json"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("invalid arguments"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn missing_source_and_program() {
    let result = execute_run_riscv_tool(r#"{}"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("source") || result.content.contains("program"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn both_source_and_program() {
    let result = execute_run_riscv_tool(r#"{"source": "fn main() {}", "program": "AAAA"}"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(result.content.contains("only one of"), "{}", result.content);
}

#[test]
#[ignore]
fn memory_size_not_aligned() {
    let result = execute_run_riscv_tool(r#"{"program": "AAAA", "memory_size": 100}"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("multiple of 4096"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn memory_size_exceeds_max() {
    let result = execute_run_riscv_tool(r#"{"program": "AAAA", "memory_size": 4198400}"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("cannot exceed 4MB"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn invalid_base64_program() {
    let result = execute_run_riscv_tool(r#"{"program": "!!!not-base64!!!"}"#, None);
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(
        result.content.contains("base64 decode"),
        "{}",
        result.content
    );
}

#[test]
#[ignore]
fn write_with_allocator() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { let mut v = alloc::vec::Vec::new(); v.push(72u8); v.push(105u8); tai::write(&v); }"}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("Hi"), "{}", result.content);
}

#[test]
#[ignore]
fn no_allocator_omits_args() {
    let result = execute_run_riscv_tool(
        r#"{"source": "fn main() { tai::write(b\"no alloc\"); }", "allocator": false}"#,
        Some(Path::new("/tmp")),
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("no alloc"), "{}", result.content);
}
