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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::process::Command;
use std::time::{Duration, Instant};
use tai_keystore::XCredentials;
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
    static mut ARGC: usize = 0;
    static mut ARGV: *const *const u8 = core::ptr::null();

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
        core::hint::unreachable_unchecked();
    }
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

const BOILERPLATE_TAIL_ALLOC: &str = r#"
use alloc::vec::Vec;

pub fn args() -> Vec<Vec<u8>> {
    unsafe {
        let argc = tai::ARGC;
        let argv = tai::ARGV;
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

fn build_boilerplate(enable_allocator: bool) -> String {
    let mut s = String::from(BOILERPLATE_HEAD);
    if enable_allocator {
        s.push_str(BOILERPLATE_ALLOC);
    }
    s.push_str(BOILERPLATE_TAIL_BASE);
    if enable_allocator {
        s.push_str(BOILERPLATE_TAIL_ALLOC);
    }
    s
}

static VM_TOOL_REGISTRY: OnceLock<Arc<ToolRegistry>> = OnceLock::new();

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
    x_credentials: Option<XCredentials>,
    cwd: Option<PathBuf>,
    output: Arc<Mutex<Vec<u8>>>,
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

                let registry = VM_TOOL_REGISTRY
                    .get()
                    .expect("VM_TOOL_REGISTRY not initialized — use ToolRegistry::build()");

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

                let exec_output = registry.execute(
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
                    self.output.lock().unwrap().extend_from_slice(&data);
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

    let target_check = Command::new("rustc")
        .arg("+nightly")
        .args(["--target", target, "--print", "target-spec-json"])
        .output()
        .map_err(|e| format!("failed to check target: {e}"))?;
    if !target_check.status.success() {
        let stderr = String::from_utf8_lossy(&target_check.stderr);
        return Err(format!(
            "RISC-V target '{target}' not available.\n{stderr}\n\
             Install with: rustup target add {target} --toolchain nightly"
        ));
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
            "link-self-contained=yes",
            "-C",
            "link-arg=-nostartfiles",
            "-C",
            "opt-level=z",
            "-o",
        ])
        .arg(&output_path)
        .arg(&input_path)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("compilation failed to start: {e}"))?;

    let timeout = Duration::from_secs(60);
    let start = Instant::now();
    let status = loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("compilation timed out after 60s".into());
        }
        match child.try_wait().map_err(|e| format!("error waiting for compiler: {e}"))? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };

    let stderr = child.stderr.take()
        .and_then(|mut pipe| {
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).ok().map(|_| buf)
        })
        .unwrap_or_default();

    if !status.success() {
        return Err(format!("compilation error:\n{}", String::from_utf8_lossy(&stderr)));
    }

    let elf = std::fs::read(&output_path)
        .map_err(|e| format!("failed to read compiled program: {e}"))?;

    drop(dir);
    Ok(elf)
}

fn run_riscv_impl(
    args: &str,
    x_credentials: Option<&XCredentials>,
    cwd: Option<&Path>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
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

    let elf = match (input.source, input.program) {
        (Some(source), None) => match compile(&source, enable_allocator) {
            Ok(elf) => elf,
            Err(e) => {
                return ToolExecutionOutput {
                    result: tool_err(e),
                    image: None,
                }
            }
        },
        (None, Some(program_b64)) => match BASE64.decode(&program_b64) {
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

    let output = Arc::new(Mutex::new(Vec::new()));
    let syscall = TaiSyscall {
        x_credentials: x_credentials.cloned(),
        cwd: cwd.map(|p| p.to_path_buf()),
        output: Arc::clone(&output),
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

    match trace.run() {
        Ok(_exit_code) => {
            let out = output.lock().unwrap().clone();
            let out_str = String::from_utf8_lossy(&out).to_string();
            ToolExecutionOutput {
                result: tool_ok(out_str),
                image: None,
            }
        }
        Err(e) => {
            let out = output.lock().unwrap().clone();
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

pub(crate) struct RunRiscV;

impl Tool for RunRiscV {
    fn name(&self) -> &'static str {
        "run_riscv"
    }

    fn description(&self) -> &'static str {
        "Compile and run Rust code in a RISC-V sandboxed virtual machine"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Rust source code providing a `fn main()` entry point. The `tai` module is auto-generated for syscall access: use `tai::write(b\"...\")`, `tai::tool_call(request, output_buffer)`, and `tai::exit(code)`."
                },
                "program": {
                    "type": "string",
                    "description": "Base64-encoded RISC-V ELF binary to execute directly (alternative to source)"
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
        x_credentials: Option<&XCredentials>,
        cwd: Option<&Path>,
    ) -> ToolExecutionOutput {
        run_riscv_impl(args, x_credentials, cwd, None)
    }

    fn execute_streaming(
        &self,
        args: &str,
        x_credentials: Option<&XCredentials>,
        cwd: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
    ) -> ToolExecutionOutput {
        run_riscv_impl(args, x_credentials, cwd, Some(output_tx))
    }
}

pub fn execute_run_riscv_tool(args: &str, cwd: Option<&Path>) -> ToolResult {
    run_riscv_impl(args, None, cwd, None).result
}

pub(crate) fn init_vm_tool_registry(registry: &Arc<ToolRegistry>) {
    let _ = VM_TOOL_REGISTRY.set(Arc::clone(registry));
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn run_riscv_rejects_invalid_json() {
        let result = run_riscv_impl(r#"not json"#, None, None, None);
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("invalid arguments"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_requires_source_or_program() {
        let result = run_riscv_impl(r#"{}"#, None, None, None);
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("source") || result.result.content.contains("program"),
            "should mention source/program: {}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_both_source_and_program() {
        let result = run_riscv_impl(
            r#"{"source": "fn main() {}", "program": "AAAA"}"#,
            None, None, None,
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("only one of"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_invalid_base64() {
        let result = run_riscv_impl(
            r#"{"program": "!!!not-base64!!!"}"#,
            None, None, None,
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("base64 decode error"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_non_aligned_memory() {
        let result = run_riscv_impl(
            r#"{"program": "AAAA", "memory_size": 100}"#,
            None, None, None,
        );
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("multiple of 4096"), "{}", result.result.content);
    }

    #[test]
    fn run_riscv_rejects_memory_over_4mb() {
        let result = run_riscv_impl(
            r#"{"program": "AAAA", "memory_size": 4198400}"#,
            None, None, None,
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
            None, None, None,
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
}
