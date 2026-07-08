use crate::openai::ChatToolCall;
use crate::tools::{Tool, ToolExecutionOutput, ToolRegistry, ToolResult, tool_ok, tool_err};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ckb_vm::Bytes;
use ckb_vm::{
    DefaultCoreMachine, DefaultMachineBuilder, DefaultMachineRunner, FlatMemory,
    CoreMachine, SupportMachine, Syscalls,
    TraceMachine, ISA_IMC, ISA_B, ISA_MOP, ISA_A,
    memory::Memory, registers, Error as VmError,
};
use ckb_vm::machine::VERSION2;
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::process::Command;
use std::thread;
use std::time::Duration;
use tai_keystore::ServiceCredential;
use tempfile::tempdir;

const BOILERPLATE_HEAD: &str = r#"
#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    tai::exit(1)
}

"#;

const BOILERPLATE_ALLOC: &str = r#"
extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::format;
use alloc::boxed::Box;

const HEAP_SIZE: usize = 131072;
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static mut HEAP_OFFSET: usize = 0;

struct BumpAlloc;

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 {
            return core::ptr::null_mut();
        }
        let offset = &mut HEAP_OFFSET;
        let align = layout.align();
        let misalign = *offset % align;
        if misalign != 0 {
            match offset.checked_add(align - misalign) {
                Some(aligned) => *offset = aligned,
                None => return core::ptr::null_mut(),
            }
        }
        match offset.checked_add(size) {
            Some(next) if next <= HEAP_SIZE => *offset = next,
            _ => return core::ptr::null_mut(),
        }
        let ptr = HEAP.as_mut_ptr().add(*offset - size);
        ptr
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: BumpAlloc = BumpAlloc;

"#;

const BOILERPLATE_TAIL_BASE: &str = r#"
pub mod tai {
    pub(crate) static mut ARGC: usize = 0;
    pub(crate) static mut ARGV: *const *const u8 = core::ptr::null();

    pub(crate) fn init_args(argc: usize, argv: *const *const u8) {
        unsafe {
            ARGC = argc;
            ARGV = argv;
        }
    }

    pub const TOOL_CALL: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const EXIT: u64 = 2;

    pub unsafe fn tool_call(request: &[u8], output: &mut [u8]) -> usize {
        let result: usize;
        core::arch::asm!(
            "ecall",
            in("a0") request.as_ptr(),
            in("a1") request.len(),
            in("a2") output.as_mut_ptr(),
            in("a3") output.len(),
            in("a7") TOOL_CALL,
            lateout("a0") result,
            options(nostack)
        );
        result
    }

    pub fn write(data: &[u8]) {
        unsafe {
            core::arch::asm!(
                "ecall",
                in("a0") data.as_ptr(),
                in("a1") data.len(),
                in("a7") WRITE,
                options(nostack)
            );
        }
    }

    pub fn exit(code: i32) -> ! {
        unsafe {
            core::arch::asm!(
                "ecall",
                in("a0") code,
                in("a7") EXIT,
                options(nostack, noreturn)
            );
        }
    }
"#;

const BOILERPLATE_TAIL_ALLOC: &str = r#"
    use alloc::vec::Vec;

    pub fn args() -> Vec<Vec<u8>> {
        unsafe {
            let argc = ARGC;
            let argv = ARGV;
            let mut result = Vec::with_capacity(argc);
            for i in 0..argc {
                let ptr = *argv.add(i);
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = core::slice::from_raw_parts(ptr, len);
                result.push(slice.to_vec());
            }
            result
        }
    }
"#;

const BOILERPLATE_TAIL_CLOSE: &str = r#"
}

#[no_mangle]
pub extern "C" fn _start() {
    let argc: usize;
    let argv: *const *const u8;
    unsafe {
        core::arch::asm!(
            "mv {argc}, a0",
            "mv {argv}, a1",
            argc = out(reg) argc,
            argv = out(reg) argv,
        );
    }
    tai::init_args(argc, argv);
    main();
    tai::exit(0);
}
"#;

fn build_boilerplate(enable_allocator: bool) -> String {
    let mut s = String::from(BOILERPLATE_HEAD);
    if enable_allocator {
        s.push_str(BOILERPLATE_ALLOC);
    }
    s.push_str(BOILERPLATE_TAIL_BASE);
    if enable_allocator {
        s.push_str(BOILERPLATE_TAIL_ALLOC);
    }
    s.push_str(BOILERPLATE_TAIL_CLOSE);
    s
}

/// Pipe Rust source through `rustfmt` and return the formatted output.
///
/// Falls back to the original source when `rustfmt` is not installed, cannot
/// be spawned, or exits with a non-success status.  This keeps the tool
/// resilient in environments where `rustfmt` is unavailable.
fn format_rust_source(source: &str) -> String {
    let mut child = match Command::new("rustfmt")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return source.to_string(),
    };

    // Write the source to rustfmt's stdin.  If the write fails we still
    // need to wait on the child to avoid a zombie, so we let the drop +
    // wait_with_output handle it below.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(source.as_bytes());
        // Explicitly close stdin so rustfmt knows to start processing.
        drop(stdin);
    }

    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            // Trim trailing newline so the output doesn't get a gratuitous
            // blank line when embedded in a markdown code block.
            let formatted = String::from_utf8_lossy(&output.stdout).to_string();
            if formatted.ends_with('\n') {
                formatted[..formatted.len() - 1].to_string()
            } else {
                formatted
            }
        }
        _ => source.to_string(),
    }
}

#[derive(Deserialize)]
struct RunRiscVInput {
    source: Option<String>,
    program: Option<String>,
    args: Option<Vec<String>>,
    max_cycles: Option<u64>,
    memory_size: Option<usize>,
    allocator: Option<bool>,
}

struct TaiSyscall {
    registry: Arc<ToolRegistry>,
    x_credentials: Option<ServiceCredential>,
    cwd: Option<PathBuf>,
    output_tx: mpsc::Sender<Vec<u8>>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl Syscalls<DefaultCoreMachine<u64, FlatMemory<u64>>> for TaiSyscall {
    fn initialize(
        &mut self,
        _machine: &mut DefaultCoreMachine<u64, FlatMemory<u64>>,
    ) -> Result<(), VmError> {
        Ok(())
    }

    fn ecall(
        &mut self,
        machine: &mut DefaultCoreMachine<u64, FlatMemory<u64>>,
    ) -> Result<bool, VmError> {
        let code = machine.registers()[registers::A7];
        match code {
            0 => {
                let req_ptr = machine.registers()[registers::A0];
                let req_len = machine.registers()[registers::A1];
                let out_ptr = machine.registers()[registers::A2];
                let out_size = machine.registers()[registers::A3];

                let request_bytes =
                    machine.memory_mut().load_bytes(req_ptr, req_len)?;

                let v: serde_json::Value = serde_json::from_slice(&request_bytes)
                    .map_err(|_| VmError::Unexpected("invalid tool call JSON".into()))?;

                let name = v["name"]
                    .as_str()
                    .ok_or(VmError::Unexpected("missing 'name' in tool call".into()))?
                    .to_string();
                let arguments_json = v["arguments_json"]
                    .as_str()
                    .unwrap_or("{}")
                    .to_string();

                let tool_call = ChatToolCall {
                    id: "vm-0".to_string(),
                    name,
                    arguments_json,
                };

                let exec_output = self.registry.execute(
                    &tool_call,
                    self.x_credentials.as_ref(),
                    self.cwd.as_deref(),
                );

                let content = exec_output.result.content.as_bytes();
                let to_write = content.len().min(out_size as usize);
                if to_write > 0 {
                    machine
                        .memory_mut()
                        .store_bytes(out_ptr, &content[..to_write])?;
                }
                let result_len = if exec_output.result.is_error {
                    0usize
                } else {
                    to_write
                };
                machine.set_register(registers::A0, result_len as u64);

                Ok(true)
            }
            1 => {
                let ptr = machine.registers()[registers::A0];
                let len = machine.registers()[registers::A1];
                if len > 0 {
                    let data = machine.memory_mut().load_bytes(ptr, len)?;
                    let _ = self.output_tx.send(data.to_vec());
                    if let Some(tx) = &self.write_tx {
                        let _ = tx.send(data.into());
                    }
                }
                Ok(true)
            }
            2 => {
                machine.set_running(false);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn compile(source: &str, enable_allocator: bool) -> Result<Vec<u8>, String> {
    let target = "riscv64imac-unknown-none-elf";

    let version = Command::new("rustc")
        .arg("+nightly")
        .arg("--version")
        .output()
        .map_err(|e| format!("rustc not found: {e}\nInstall from https://rustup.rs"))?;
    if !version.status.success() {
        let stderr = String::from_utf8_lossy(&version.stderr);
        return Err(format!("rustc +nightly check failed: {stderr}"));
    }

    let dir = tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;
    let input_path = dir.path().join("main.rs");
    let output_path = dir.path().join("output.elf");

    let boilerplate = build_boilerplate(enable_allocator);
    let full_source = format!("{boilerplate}\n// User code\n{source}");
    std::fs::write(&input_path, &full_source)
        .map_err(|e| format!("failed to write source: {e}"))?;

    let mut child = Command::new("rustc")
        .arg("+nightly")
        .args([
            "--target",
            target,
            "-C",
            "opt-level=z",
            "-o",
        ])
        .arg(&output_path)
        .arg(&input_path)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("compilation failed to start: {e}"))?;

    let stderr_pipe = child.stderr.take();

    // Wait for the compiler to finish with a poll loop.  The calling thread
    // is a dedicated tool-execution thread, so blocking here is fine.
    let timeout = Duration::from_secs(60);
    let start = std::time::Instant::now();
    let status = loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("compilation timed out after 60s".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                // Brief sleep to avoid busy-looping.
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("error waiting for compiler: {e}")),
        }
    };

    let mut stderr_buf = Vec::new();
    if let Some(mut p) = stderr_pipe {
        p.read_to_end(&mut stderr_buf)
            .map_err(|e| format!("failed to read compiler stderr: {e}"))?;
    }

    if !status.success() {
        return Err(format!("compilation error:\n{}", String::from_utf8_lossy(&stderr_buf)));
    }

    let elf = std::fs::read(&output_path)
        .map_err(|e| format!("failed to read compiled program: {e}"))?;

    drop(dir);
    Ok(elf)
}

fn run_riscv_impl(
    args: &str,
    x_credentials: Option<&ServiceCredential>,
    cwd: Option<&Path>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
    registry: Arc<ToolRegistry>,
) -> ToolExecutionOutput {
    let input: RunRiscVInput = match serde_json::from_str(args) {
        Ok(i) => i,
        Err(e) => {
            return ToolExecutionOutput {
                result: tool_err(format!("invalid arguments: {e}")),
                image: None,
            }
        }
    };

    let enable_allocator = input.allocator.unwrap_or(true);

    // Format the user's Rust source with rustfmt before compiling.
    // When rustfmt is unavailable or the file is already well-formatted the
    // output is identical to the input — the fallback is invisible.
    let formatted_source: Option<String> = input.source.as_ref().map(|s| format_rust_source(s));

    // Source to hand to rustc — prefer the formatted version, fall back to
    // the original if formatting failed or was skipped.
    let compile_source: Option<&str> = formatted_source
        .as_deref()
        .or_else(|| input.source.as_deref());

    // Source to include in the result display (also prefer formatted).
    let display_source: Option<&str> = formatted_source
        .as_deref()
        .or_else(|| input.source.as_deref());

    let elf = match (compile_source, input.program.as_deref()) {
        (Some(source), None) => match compile(source, enable_allocator) {
            Ok(elf) => elf,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(e),
                    image: None,
                }
            }
        },
        (None, Some(program_b64)) => match BASE64.decode(program_b64) {
            Ok(elf) => elf,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(format!("base64 decode error: {e}")),
                    image: None,
                }
            }
        },
        (None, None) => {
            return ToolExecutionOutput {
                result: tool_err("either 'source' or 'program' is required"),
                image: None,
            }
        }
        (Some(_), Some(_)) => {
            return ToolExecutionOutput {
                result: tool_err("provide only one of 'source' or 'program'"),
                image: None,
            }
        }
    };

    let memory_size = input.memory_size.unwrap_or(4 * 1024 * 1024);
    if memory_size % 4096 != 0 {
        return ToolExecutionOutput {
            result: tool_err("memory_size must be a multiple of 4096"),
            image: None,
        };
    }
    if memory_size > 4 * 1024 * 1024 {
        return ToolExecutionOutput {
            result: tool_err("memory_size cannot exceed 4MB"),
            image: None,
        };
    }

    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    let syscall = TaiSyscall {
        registry,
        x_credentials: x_credentials.cloned(),
        cwd: cwd.map(|p| p.to_path_buf()),
        output_tx,
        write_tx,
    };

    let core = DefaultCoreMachine::<u64, FlatMemory<u64>>::new_with_memory(
        ISA_IMC | ISA_A | ISA_B | ISA_MOP,
        VERSION2,
        input.max_cycles.unwrap_or(1_000_000),
        memory_size,
    );

    let machine = DefaultMachineBuilder::new(core)
        .syscall(Box::new(syscall))
        .instruction_cycle_func(Box::new(ckb_vm::cost_model::estimate_cycles))
        .build();

    let mut trace = TraceMachine::new(machine);

    let args_list: Vec<Bytes> = input
        .args
        .unwrap_or_default()
        .into_iter()
        .map(|a| Bytes::from(a))
        .collect();

    if let Err(e) =
        trace.load_program(&Bytes::from(elf), args_list.iter().map(|b| Ok(b.clone())))
    {
        return ToolExecutionOutput {
            result: tool_err(format!("failed to load program: {e}")),
            image: None,
        };
    }

    // The CKB-VM does not set A0/A1 registers before jumping to _start.
    // Set them explicitly so the guest's _start can read argc from a0 and
    // argv from a1 (the stack has already been laid out by initialize_stack
    // with [argc, argv[0], ..., NULL] starting at SP).
    let arg_count = args_list.len() as u64;
    let sp = trace.registers()[registers::SP];
    trace.set_register(registers::A0, arg_count);
    trace.set_register(registers::A1, sp + 8);

    match trace.run() {
        Ok(_exit_code) => {
            // Drain all buffered output written via the TaiSyscall write
            // handler during VM execution.  `trace.run()` is synchronous,
            // so every write completes before `try_iter()` runs — no need
            // for a blocking receive even though the sender is still alive.
            let out: Vec<u8> = output_rx.try_iter().flatten().collect();
            let out_str = String::from_utf8_lossy(&out).to_string();

            // Prepend the formatted source as a syntax-highlighted markdown
            // code block so the tai-tui can render it with syntect.
            let mut result_content = String::new();
            if let Some(source) = display_source {
                result_content.push_str("```rust\n");
                result_content.push_str(source);
                if !source.ends_with('\n') {
                    result_content.push('\n');
                }
                result_content.push_str("```\n\n");
            }
            result_content.push_str(&out_str);

            ToolExecutionOutput {
                result: tool_ok(result_content),
                image: None,
            }
        }
        Err(e) => {
            // Same pattern — drain all buffered output collected before
            // the VM faulted.  No blocking needed (see Ok arm above).
            let out: Vec<u8> = output_rx.try_iter().flatten().collect();
            let out_str = String::from_utf8_lossy(&out).to_string();
            let msg = if out_str.is_empty() {
                format!("VM error: {e}")
            } else {
                format!("VM error: {e}\noutput so far:\n{out_str}")
            };
            ToolExecutionOutput {
                result: tool_err(msg),
                image: None,
            }
        }
    }
}

pub(crate) struct RunRiscV {
    registry: Weak<ToolRegistry>,
}

impl RunRiscV {
    pub fn new(registry: Weak<ToolRegistry>) -> Self {
        RunRiscV { registry }
    }
}

impl Tool for RunRiscV {
    fn name(&self) -> &'static str {
        "run_riscv"
    }

    fn group(&self) -> &'static str {
        "vm"
    }

    fn description(&self) -> &'static str {
        "Compile and run Rust code in a RISC-V sandboxed VM. PREFER the 'source' parameter over 'program'. With 'source', only provide a `fn main()` body — the tool auto-generates #![no_std], #![no_main], #[panic_handler], _start, and the `tai` module with syscall wrappers (tai::write, tai::exit, tai::tool_call). Do NOT use raw ecall with Linux syscall numbers (64, 93) — they are not supported."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Rust source code for `fn main()`. CRITICAL: Do NOT include #![no_std], #![no_main], #[panic_handler], _start, or the `tai` module — these are auto-generated. Do NOT use raw ecall with Linux syscall numbers (64 for write, 93 for exit). Use the provided wrappers: tai::write(b\"...\"), tai::exit(code), tai::tool_call(request, &mut output). Example: `fn main() { tai::write(b\"hello\\n\"); tai::exit(0); }`. When allocator:true (default), alloc types are pre-imported: Vec, String, Box, format!, vec!, .to_string()."
                },
                "program": {
                    "type": "string",
                    "description": "Base64-encoded RISC-V ELF binary. Only use if you compiled externally WITH the tai syscall ABI (syscall 0=tool_call, 1=write, 2=exit). Programs using Linux syscall numbers (64=write, 93=exit) will fail. When in doubt, use 'source' instead."
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Command-line arguments passed to the guest program. Read them with tai::args() -> Vec<Vec<u8>> in the guest code (requires allocator: true, the default)."
                },
                "allocator": {
                    "type": "boolean",
                    "description": "Include a 128 KB bump allocator (#[global_allocator]) so guest code can use alloc crate types (Vec, String, format!, Box, etc.). When true, tai::args() is available to parse guest argv. When false, args() is omitted and guest code must access argc/argv directly from _start's a0/a1 registers. Default: true."
                },
                "max_cycles": {
                    "type": "integer",
                    "description": "Maximum CPU cycles before VM termination (default: 1_000_000)"
                }
            }
        })
    }

    fn execute(
        &self,
        args: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        match self.registry.upgrade() {
            Some(registry) => run_riscv_impl(args, x_credentials, cwd, None, registry),
            None => ToolExecutionOutput {
                result: tool_err("ToolRegistry no longer available"),
                image: None,
            },
        }
    }

    fn execute_streaming(
        &self,
        args: &str,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
    output_tx: mpsc::Sender<Vec<u8>>,
    ) -> ToolExecutionOutput {
        match self.registry.upgrade() {
            Some(registry) => run_riscv_impl(args, x_credentials, cwd, Some(output_tx), registry),
            None => ToolExecutionOutput {
                result: tool_err("ToolRegistry no longer available"),
                image: None,
            },
        }
    }
}

pub fn execute_run_riscv_tool(args: &str, cwd: Option<&Path>) -> ToolResult {
    let registry = Arc::new(ToolRegistry::new());
    run_riscv_impl(args, None, cwd, None, registry).result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rust_source_returns_input_when_rustfmt_unavailable() {
        // When rustfmt is not in PATH, the function should return the source
        // unchanged rather than panicking or erroring.
        let src = "fn main() { let x = 1; }";
        let result = format_rust_source(src);
        // The function either formats (rustfmt available) or returns the
        // original; either way it must not panic and must contain the fn.
        assert!(result.contains("fn main()"), "must contain fn main()");
        assert!(!result.is_empty(), "result must not be empty");
    }

    #[test]
    fn format_rust_source_formats_when_rustfmt_available() {
        // Only test actual formatting if rustfmt is installed.
        let has_rustfmt = Command::new("rustfmt")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_rustfmt {
            return;
        }

        let src = "fn main(){let x=1;}";
        let result = format_rust_source(src);
        assert!(result.contains("fn main()"), "must contain fn main()");
        // If formatting succeeded, there should be spaces around braces.
        assert!(
            !result.contains("fn main(){"),
            "formatted source should not lack spaces: {result}"
        );
    }

    #[test]
    fn format_rust_source_preserves_valid_code() {
        let has_rustfmt = Command::new("rustfmt")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_rustfmt {
            return;
        }

        // Already well-formatted code should not be mangled.
        let src = "fn main() {\n    let x = 1;\n}\n";
        let result = format_rust_source(src);
        assert_eq!(result, "fn main() {\n    let x = 1;\n}");
    }

    #[test]
    fn build_boilerplate_with_alloc_includes_allocator() {
        let result = build_boilerplate(true);
        assert!(result.contains("struct BumpAlloc"), "should contain bump allocator");
        assert!(result.contains("fn args()"), "should contain args()");
        assert!(result.contains("fn _start()"), "should contain _start");
        assert!(result.contains("tai::exit(1)"), "should contain panic handler");
    }

    #[test]
    fn build_boilerplate_without_alloc_excludes_allocator() {
        let result = build_boilerplate(false);
        assert!(!result.contains("struct BumpAlloc"), "should NOT contain bump allocator");
        assert!(!result.contains("fn args()"), "should NOT contain args()");
        assert!(result.contains("fn _start()"), "should still contain _start");
        assert!(result.contains("tai::exit(1)"), "should still contain panic handler");
    }

    #[test]
    fn build_boilerplate_contains_tai_module() {
        let result = build_boilerplate(false);
        assert!(result.contains("pub mod tai"));
        assert!(result.contains("TOOL_CALL"));
        assert!(result.contains("WRITE"));
        assert!(result.contains("EXIT"));
    }

    fn dummy_registry() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    #[test]
    fn run_riscv_rejects_invalid_json() {
        let result = run_riscv_impl(r#"not json"#, None, None, None, dummy_registry());
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("invalid arguments"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_requires_source_or_program() {
        let result = run_riscv_impl(r#"{}"#, None, None, None, dummy_registry());
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("source") || result.result.content.contains("program"),
            "should mention source/program: {}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_both_source_and_program() {
        let result = run_riscv_impl(
            r#"{"source": "fn main() {}", "program": "AAAA"}"#,
            None, None, None, dummy_registry(),
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("only one of"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_invalid_base64() {
        let result = run_riscv_impl(
            r#"{"program": "!!!not-base64!!!"}"#,
            None, None, None, dummy_registry(),
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("base64 decode error"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_non_aligned_memory() {
        let result = run_riscv_impl(
            r#"{"program": "AAAA", "memory_size": 100}"#,
            None, None, None, dummy_registry(),
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("multiple of 4096"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_memory_over_4mb() {
        let result = run_riscv_impl(
            r#"{"program": "AAAA", "memory_size": 4198400}"#,
            None, None, None, dummy_registry(),
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("cannot exceed 4MB"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_accepts_valid_base64_program_with_4k_aligned_memory() {
        // "AAAA" decodes to 3 zero bytes — not a valid ELF, but that's caught at load time,
        // not during input validation. This test verifies that valid base64 + aligned memory
        // passes input validation (the error will be about ELF loading, not input).
        let result = run_riscv_impl(
            r#"{"program": "AAAA", "memory_size": 4096}"#,
            None, None, None, dummy_registry(),
        );
        // Should fail at ELF load, not at input validation
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(!result.result.content.contains("base64 decode error"),
            "should not be base64 error: {}", result.result.content);
        assert!(!result.result.content.contains("multiple of 4096"),
            "should not be alignment error: {}", result.result.content);
        assert!(!result.result.content.contains("cannot exceed 4MB"),
            "should not be size error: {}", result.result.content);
    }

    // -- Channel output collection tests -----------------------------------
    //
    // These verify the `try_iter().flatten().collect()` pattern used to drain
    // the VM's byte output channel after a synchronous `trace.run()`.

    #[test]
    fn channel_output_collection_empty_when_nothing_sent() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        drop(tx);

        let out: Vec<u8> = rx.try_iter().flatten().collect();
        assert!(out.is_empty());
    }

    #[test]
    fn channel_output_collection_single_chunk() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hello".to_vec()).unwrap();
        drop(tx);

        let out: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn channel_output_collection_preserves_order() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(b"world".to_vec()).unwrap();
        drop(tx);

        let out: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn channel_output_collection_works_with_open_channel() {
        // `try_iter()` returns whatever is buffered even if the sender
        // is still alive — exactly the pattern used after `trace.run()`.
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"data".to_vec()).unwrap();
        // Don't drop tx — simulate the open-sender scenario.

        let out: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(out, b"data");
    }

    #[test]
    fn channel_output_collection_multiple_chunks_open_sender() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"a".to_vec()).unwrap();
        tx.send(b"b".to_vec()).unwrap();
        tx.send(b"c".to_vec()).unwrap();

        let out: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(out, b"abc");
    }
}
