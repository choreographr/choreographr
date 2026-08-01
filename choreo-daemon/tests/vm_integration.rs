use choreographr::{RunRiscVInput, execute_run_riscv_tool};
use std::path::Path;

#[test]
#[ignore]
fn simple_write() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { choreo::write(b\"Hello from VM!\"); }".to_string()),
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
            source: Some("fn main() { choreo::exit(0); }".to_string()),
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
                "fn main() { let args = choreo::args(); if args.len() > 0 { choreo::write(&args[0]); } }"
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
fn program_path_missing_file() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program_path: Some("/nonexistent/definitely-not-here-choreo-9f3c2.elf".to_string()),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cannot read program file"), "{}", err);
}

#[test]
#[ignore]
fn program_path_runs_precompiled_elf() {
    // Compile a minimal no_std ELF externally (as a user would with rustc),
    // then verify the VM runs it straight from disk via `program_path`.
    let dir = std::env::temp_dir().join(format!(
        "choreo-vm-path-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let src_path = dir.join("guest.rs");
    let elf_path = dir.join("guest.elf");

    // Guest: write "hello from file" via choreographr WRITE syscall (1),
    // then exit with code 42 via Linux exit syscall (93, handled natively by
    // CKB-VM's DefaultMachine). No allocator needed — no heap usage.
    let src = r#"
#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let msg = b"hello from file";
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a0") msg.as_ptr(),
            in("a1") msg.len(),
            in("a7") 1u64,
            options(nostack)
        );
        core::arch::asm!(
            "ecall",
            in("a0") 42u64,
            in("a7") 93u64,
            options(noreturn)
        );
    }
}
"#;
    std::fs::write(&src_path, src).expect("write guest source");

    let status = std::process::Command::new("rustc")
        .arg("+stable")
        .args(["--target", "riscv64imac-unknown-none-elf"])
        .args(["-C", "opt-level=z", "--edition", "2024"])
        .arg("-o")
        .arg(&elf_path)
        .arg(&src_path)
        .status()
        .expect("spawn rustc");
    assert!(status.success(), "external rustc compile failed");

    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program_path: Some(elf_path.to_string_lossy().to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello from file"), "{}", content);
    assert!(
        content.contains("exited with code 42"),
        "expected exit banner: {}",
        content
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn write_with_vec() {
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some("fn main() { let mut v = Vec::new(); v.push(72u8); v.push(105u8); choreo::write(&v); }".to_string()),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("Hi"), "{}", content);
}
