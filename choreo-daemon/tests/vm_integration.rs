use choreo_daemon::{RunRiscVInput, execute_run_riscv_tool};
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
    // One page over ckb-vm's hard cap (RISCV_MAX_MEMORY = 4MB in 0.24.14).
    // The tool must reject this with a clean validation error — a larger
    // size would panic inside FlatMemory::new_with_memory instead.
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program: Some("AAAA".to_string()),
            memory_size: Some(4 * 1024 * 1024 + 4096),
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
fn memory_size_at_cap_passes_validation() {
    // memory_size == ckb-vm's RISCV_MAX_MEMORY (4MB) is the largest valid
    // size; validation must accept it and only fail later at ELF load time
    // ("AAAA" is not a real ELF).
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            program: Some("AAAA".to_string()),
            memory_size: Some(4 * 1024 * 1024),
            ..Default::default()
        },
        None,
    );
    assert!(result.is_err(), "expected error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(!err.contains("cannot exceed 4MB"), "{}", err);
    assert!(!err.contains("multiple of 4096"), "{}", err);
    assert!(err.contains("failed to load program"), "{}", err);
}

#[test]
#[ignore]
fn bitmanip_source_program() {
    // The daemon compiles `source` guests with -C opt-level=2 -C target-feature=+b,-a
    // (the `-a` disables the A/atomic extension; see atomic_guest_rejected_at_compile_time).
    // The arg is runtime data (defeating const folding), so LLVM must emit real
    // bitmanip instructions — `count_ones` -> cpop, `leading_zeros` -> clz,
    // `trailing_zeros` -> ctz, `swap_bytes` -> rev8 (verified via objdump: all
    // four appear only in the +b build). Correct results prove ckb-vm decodes
    // and executes the B-extension instructions.
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some(
                "fn main() {
                    let raw = choreo::args();
                    let s = String::from_utf8_lossy(&raw[0]).to_string();
                    let n: u64 = s.parse().unwrap_or(0);
                    // 0x0F0F_0F0F_0000_0000 — runtime value, not const-foldable.
                    let a = n << 32;
                    let out = format!(
                        \"cpop={} clz={} ctz={} rev8={:016x}\",
                        a.count_ones(),
                        a.leading_zeros(),
                        a.trailing_zeros(),
                        a.swap_bytes(),
                    );
                    choreo::write(out.as_bytes());
                }"
                .to_string(),
            ),
            args: Some(vec!["252645135".to_string()]), // 0x0F0F_0F0F
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    let content = result.unwrap_or_default();
    assert!(
        content.contains("cpop=16 clz=4 ctz=32 rev8=000000000f0f0f0f"),
        "{}",
        content
    );
}

#[test]
#[ignore]
fn atomic_guest_rejected_at_compile_time() {
    // The daemon compiles `source` guests with -C opt-level=2 -C target-feature=+b,-a:
    // the RISC-V A (atomic) extension is disabled because the VM is single-hart
    // (one instruction stream), so atomics have no real concurrency semantics.
    // A guest using an RMW atomic lowers to `amoadd.w`, which LLVM cannot select
    // without the A extension — the compile fails in the LLVM backend before the
    // VM ever runs. The choreo::write below is intentionally unreachable.
    let result = execute_run_riscv_tool(
        &RunRiscVInput {
            source: Some(
                "use core::sync::atomic::{AtomicU32, Ordering};
                 fn main() {
                     let a = AtomicU32::new(0);
                     a.fetch_add(1, Ordering::Relaxed);
                     choreo::write(b\"unreachable\");
                 }"
                .to_string(),
            ),
            ..Default::default()
        },
        Some(Path::new("/tmp")),
    );
    assert!(result.is_err(), "expected compile error: {:?}", result);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("compilation error") || err.contains("LLVM"),
        "expected LLVM backend compile failure: {}",
        err
    );
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
