use std::path::Path;
use tai_daemon::{RunRiscVInput, execute_run_riscv_tool};

#[test]
#[ignore]
fn simple_write() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { tai::write(b\"Hello from VM!\"); }".to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("Hello from VM!"), "{}", content);
}

#[test]
#[ignore]
fn exit_zero() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { tai::exit(0); }".to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    // VM should exit cleanly and show the exit banner.
    assert!(
        content.contains("exited with code 0"),
        "expected exit banner: {}",
        content
    );
}

#[test]
#[ignore]
fn with_args() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some(
                "fn main() { let args = tai::args(); if args.len() > 0 { tai::write(&args[0]); } }"
                    .to_string(),
            ),
            args: Some(vec!["hello".to_string()]),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello"), "{}", content);
}

#[test]
#[ignore]
fn cycle_limit_enforced() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { loop {} }".to_string()),
            max_cycles: Some(100),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cycle limit") || err.contains("VM error"),
        "expected cycle limit or VM error: {}",
        err
    );
}

#[test]
#[ignore]
fn compilation_error_invalid_rust() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { this is bad rust }".to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("compilation error") || err.contains("compile"),
        "expected compilation error: {}",
        err
    );
}

#[test]
#[ignore]
fn missing_source_and_program() {
    let result = execute_run_riscv_tool(&RunRiscVInput::default(), None);
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("source") || err.contains("program"), "{}", err);
}

#[test]
#[ignore]
fn both_source_and_program() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() {}".to_string()),
            program: Some("AAAA".to_string()),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("only one of"), "{}", err);
}

#[test]
#[ignore]
fn memory_size_not_aligned() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program: Some("AAAA".to_string()),
            memory_size: Some(100),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("multiple of 4096"), "{}", err);
}

#[test]
#[ignore]
fn memory_size_exceeds_max() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program: Some("AAAA".to_string()),
            memory_size: Some(4198400),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cannot exceed 4MB"), "{}", err);
}

#[test]
#[ignore]
fn invalid_base64_program() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program: Some("!!!not-base64!!!".to_string()),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("base64 decode"), "{}", err);
}

#[test]
#[ignore]
fn write_with_vec() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { let mut v = Vec::new(); v.push(72u8); v.push(105u8); tai::write(&v); }".to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("Hi"), "{}", content);
}
