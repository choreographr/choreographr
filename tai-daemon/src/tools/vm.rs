use crate::tools::{
    Tool, ToolError, ToolOutput, ToolRegistry, context::ToolContext, tool_err, tool_ok,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ckb_vm::Bytes;
use ckb_vm::machine::VERSION2;
use ckb_vm::{
    CoreMachine, DefaultCoreMachine, DefaultMachineBuilder, DefaultMachineRunner, Error as VmError,
    FlatMemory, ISA_A, ISA_B, ISA_IMC, ISA_MOP, SupportMachine, Syscalls, TraceMachine,
    memory::Memory, registers,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
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
use core::ptr;

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
        // Use addr_of_mut! to avoid edition 2024 static_mut_refs error.
        let offset = ptr::addr_of_mut!(HEAP_OFFSET);
        let align = layout.align();
        let misalign = *offset % align;
        if misalign != 0 {
            match (*offset).checked_add(align - misalign) {
                Some(aligned) => *offset = aligned,
                None => return core::ptr::null_mut(),
            }
        }
        match (*offset).checked_add(size) {
            Some(next) if next <= HEAP_SIZE => *offset = next,
            _ => return core::ptr::null_mut(),
        }
        let heap_ptr = ptr::addr_of_mut!(HEAP) as *mut u8;
        heap_ptr.add(*offset - size)
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
    pub const BATCH_TOOL_CALL: u64 = 3;

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

    /// Submit multiple tool calls for concurrent execution on the host.
    /// Request format is a postcard frame: [count: varint][name: str][args: bytes]*.
    /// Response format is: [count: varint][result]*
    /// where each result is a postcard-encoded `Result<Vec<u8>, String>`.
    pub unsafe fn batch_tool_call(request: &[u8], output: &mut [u8]) -> usize {
        let result: usize;
        core::arch::asm!(
            "ecall",
            in("a0") request.as_ptr(),
            in("a1") request.len(),
            in("a2") output.as_mut_ptr(),
            in("a3") output.len(),
            in("a7") BATCH_TOOL_CALL,
            lateout("a0") result,
            options(nostack)
        );
        result
    }
"#;

const BOILERPLATE_TAIL_ALLOC: &str = r#"
    use alloc::vec::Vec;
    use alloc::string::String;
    use alloc::string::ToString;

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

#[unsafe(no_mangle)]
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

const BOILERPLATE_TAIL_ENCODING: &str = r#"
    // ── Postcard-format encoding helpers ──────────────────────────────

    /// Encode a u64 as a postcard varint.
    pub fn enc_varint(mut v: u64, buf: &mut Vec<u8>) {
        loop {
            if v < 0x80 {
                buf.push(v as u8);
                break;
            }
            buf.push((v as u8) | 0x80);
            v >>= 7;
        }
    }

    /// Encode a string as postcard: varint(len) + UTF-8 bytes.
    pub fn enc_str(s: &str, buf: &mut Vec<u8>) {
        enc_varint(s.len() as u64, buf);
        buf.extend_from_slice(s.as_bytes());
    }

    /// Encode bytes as postcard: varint(len) + raw bytes.
    pub fn enc_bytes(b: &[u8], buf: &mut Vec<u8>) {
        enc_varint(b.len() as u64, buf);
        buf.extend_from_slice(b);
    }

    /// Encode a u64 as a postcard varint.
    pub fn enc_u64(v: u64, buf: &mut Vec<u8>) {
        enc_varint(v, buf);
    }

    /// Encode an Option<&str>: 0x00 for None, 0x01 + enc_str for Some.
    pub fn enc_option_str(v: Option<&str>, buf: &mut Vec<u8>) {
        match v {
            Some(s) => {
                buf.push(0x01);
                enc_str(s, buf);
            }
            None => buf.push(0x00),
        }
    }

    /// Encode an Option<u64>: 0x00 for None, 0x01 + enc_varint for Some.
    pub fn enc_option_u64(v: Option<u64>, buf: &mut Vec<u8>) {
        match v {
            Some(n) => {
                buf.push(0x01);
                enc_varint(n, buf);
            }
            None => buf.push(0x00),
        }
    }

    /// Encode a bool as 1 byte.
    pub fn enc_bool(v: bool, buf: &mut Vec<u8>) {
        buf.push(v as u8);
    }

    /// Encode an Option<bool>: 0x00 for None, 0x01 + 1 byte for Some.
    pub fn enc_option_bool(v: Option<bool>, buf: &mut Vec<u8>) {
        match v {
            Some(b) => {
                buf.push(0x01);
                buf.push(b as u8);
            }
            None => buf.push(0x00),
        }
    }

    /// Decode a postcard varint from the front of a byte slice.
    /// Returns (value, bytes_consumed).
    pub fn dec_varint(buf: &[u8]) -> Result<(u64, usize), &'static str> {
        let mut value: u64 = 0;
        let mut shift: u64 = 0;
        let mut consumed: usize = 0;
        for &byte in buf {
            value |= ((byte & 0x7F) as u64) << shift;
            consumed += 1;
            if byte & 0x80 == 0 {
                return Ok((value, consumed));
            }
            shift += 7;
            if shift > 63 {
                return Err("varint too large");
            }
        }
        Err("unterminated varint")
    }

    /// Decode a postcard string from the front of a byte slice.
    pub fn dec_str<'a>(buf: &'a [u8]) -> Result<(&'a str, usize), &'static str> {
        let (len, consumed) = dec_varint(buf)?;
        let start = consumed;
        let end = start + len as usize;
        if end > buf.len() {
            return Err("string too short");
        }
        let s = core::str::from_utf8(&buf[start..end])
            .map_err(|_| "invalid utf-8")?;
        Ok((s, end))
    }

    /// Decode a postcard `Result<Vec<u8>, String>` from a byte slice.
    /// Returns Ok(bytes) or Err(error_string).
    pub fn dec_result(resp: &[u8]) -> Result<&[u8], &str> {
        if resp.is_empty() { return Err("empty response"); }
        let status = resp[0];
        let rest = &resp[1..];
        let (payload_len, consumed) = dec_varint(rest)?;
        let start = consumed;
        let end = start + payload_len as usize;
        if end > rest.len() {
            return Err("truncated payload");
        }
        let payload = &rest[start..end];
        match status {
            0 => Ok(payload),       // Ok
            1 => Err(core::str::from_utf8(payload).unwrap_or("decode error")), // Err
            _ => Err("unknown result status"),
        }
    }

    /// Like `dec_result` but also returns the total number of bytes consumed
    /// so callers can advance a cursor across a sequence of results.
    pub fn dec_result_raw(data: &[u8]) -> Result<(&[u8], usize), &str> {
        if data.is_empty() { return Err("empty"); }
        let status = data[0];
        let (payload_len, mut consumed) = dec_varint(&data[1..])?;
        consumed += 1; // account for the status byte
        let payload_start = consumed;
        let payload_end = payload_start + payload_len as usize;
        if payload_end > data.len() { return Err("truncated payload"); }
        let payload = &data[payload_start..payload_end];
        match status {
            0 => Ok((payload, payload_end)),
            1 => Err(core::str::from_utf8(payload).map_err(|_| "invalid utf-8")?),
            _ => Err("unknown result status"),
        }
    }

    // ── Tool call helpers ─────────────────────────────────────────────

    /// Make a raw tool call. Encodes tool name + args as postcard frame,
    /// does ecall, returns the raw result bytes.
    pub fn call(name: &str, args: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        enc_str(name, &mut buf);
        buf.extend_from_slice(args);
        let mut output = Vec::new();
        output.resize(65536, 0u8);
        let n = unsafe { tool_call(&buf, &mut output) };
        output[..n].to_vec()
    }

    /// Submit multiple tool calls for concurrent execution on the host.
    ///
    /// Encodes all requests into a single batch frame, issues one ecall,
    /// and returns results in submission order. Each result is independent —
    /// one tool may fail without affecting the others.
    pub fn call_multi(requests: &[(&str, &[u8])]) -> Vec<Result<Vec<u8>, &str>> {
        let mut buf = Vec::new();
        enc_varint(requests.len() as u64, &mut buf);
        for (name, args) in requests {
            enc_str(name, &mut buf);
            enc_bytes(args, &mut buf);
        }
        let mut output = Vec::new();
        output.resize(65536, 0u8);
        let n = unsafe { batch_tool_call(&buf, &mut output) };
        let data = &output[..n];
        let (count, mut pos) = match dec_varint(data) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut results = Vec::with_capacity(count as usize);
        for _ in 0..count {
            if pos >= data.len() { break; }
            match dec_result_raw(&data[pos..]) {
                Ok((payload, end)) => {
                    results.push(Ok(payload.to_vec()));
                    pos = end;
                }
                Err(e) => {
                    // Decoding failed mid-stream; return partial results and stop.
                    // The host always produces well-formed responses, so hitting
                    // this path indicates a fundamental protocol mismatch.
                    results.push(Err(e));
                    break;
                }
            }
        }
        results
    }

    // ── Shell variants (mirrors host Shell enum) ─────────────────────

    /// Shell variants matching the host Shell enum for `sh()`.
    pub enum Shell {
        Bash,
        Dash,
        Zsh,
    }

    // ── Per-tool wrappers ─────────────────────────────────────────────

    /// db_get(key: &str) -> raw value bytes. Empty vec = not found or error.
    pub fn db_get(key: &str) -> Vec<u8> {
        let mut args = Vec::new();
        enc_str(key, &mut args);
        let resp = call("db_get", &args);
        dec_result(&resp).unwrap_or(&[]).to_vec()
    }

    /// db_set(key: &str, value: &[u8]).
    pub fn db_set(key: &str, value: &[u8]) {
        let mut args = Vec::new();
        enc_str(key, &mut args);
        enc_bytes(value, &mut args);
        let _resp = call("db_set", &args);
    }

    /// db_delete(key: &str) -> bool (true if deleted).
    pub fn db_delete(key: &str) -> bool {
        let mut args = Vec::new();
        enc_str(key, &mut args);
        let resp = call("db_delete", &args);
        dec_result(&resp).is_ok()
    }

    /// read_file(path: &str) -> file content as String.
    pub fn read_file(path: &str) -> String {
        let mut args = Vec::new();
        enc_str(path, &mut args);
        let resp = call("read_file", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// write_file(path: &str, content: &str, overwrite: bool).
    pub fn write_file(path: &str, content: &str, overwrite: bool) {
        let mut args = Vec::new();
        enc_str(path, &mut args);
        enc_str(content, &mut args);
        enc_bool(overwrite, &mut args);
        let _resp = call("write_file", &args);
    }

    /// git_status() -> status string.
    pub fn git_status() -> String {
        let resp = call("git_status", &[]);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// grep(pattern: &str) -> file content search results as string.
    pub fn grep(pattern: &str) -> String {
        let mut args = Vec::new();
        enc_str(pattern, &mut args);
        enc_bool(false, &mut args);       // regex (default: literal)
        enc_option_str(None, &mut args);  // include
        enc_option_str(None, &mut args);  // path
        enc_option_u64(None, &mut args);  // max_results
        let resp = call("grep", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// find(pattern: &str) -> file name search results as string.
    pub fn find(pattern: &str) -> String {
        let mut args = Vec::new();
        enc_str(pattern, &mut args);
        enc_bool(false, &mut args);       // glob (default: substring)
        enc_option_str(None, &mut args);  // path
        enc_option_u64(None, &mut args);  // max_results
        let resp = call("find", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// http_request(method: &str, url: &str, body: Option<&str>, timeout_secs: Option<u64>) -> response string.
    /// Headers can be passed as a slice of (key, value) pairs.
    pub fn http_request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>, timeout_secs: Option<u64>) -> String {
        let mut args = Vec::new();
        enc_str(method, &mut args);
        enc_str(url, &mut args);
        // headers: Vec<(String, String)> — postcard encodes as varint(len) + each pair
        enc_varint(headers.len() as u64, &mut args);
        for &(k, v) in headers {
            enc_str(k, &mut args);
            enc_str(v, &mut args);
        }
        enc_option_str(body, &mut args);
        enc_option_u64(timeout_secs, &mut args);
        let resp = call("http_request", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// sh(command: &str, shell: Shell) -> command output string.
    /// Execute a shell command using the specified POSIX-compatible shell.
    pub fn sh(command: &str, shell: Shell, workdir: Option<&str>, timeout_ms: Option<u64>) -> String {
        let mut args = Vec::new();
        enc_str(command, &mut args);
        // Encode Shell as a postcard unit variant index (0 = Bash, 1 = Dash, 2 = Zsh)
        enc_varint(shell as u64, &mut args);
        enc_option_str(workdir, &mut args);
        enc_option_u64(timeout_ms, &mut args);
        let resp = call("sh", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
        }
    }

    /// exec(command: &str, cmd_args: &[&str], workdir: Option<&str>, timeout_ms: Option<u64>) -> command output string.
    pub fn exec(command: &str, cmd_args: &[&str], workdir: Option<&str>, timeout_ms: Option<u64>) -> String {
        let mut args = Vec::new();
        enc_str(command, &mut args);
        // args_list: Vec<String> — postcard encodes as varint(len) + each string
        enc_varint(cmd_args.len() as u64, &mut args);
        for a in cmd_args {
            enc_str(a, &mut args);
        }
        enc_option_str(workdir, &mut args);
        enc_option_u64(timeout_ms, &mut args);
        let resp = call("exec", &args);
        match dec_result(&resp) {
            Ok(b) => String::from_utf8_lossy(b).to_string(),
            Err(_) => String::new(),
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
        s.push_str(BOILERPLATE_TAIL_ENCODING);
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

#[derive(Default, Deserialize, Serialize, JsonSchema)]
pub struct RunRiscVInput {
    pub source: Option<String>,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub max_cycles: Option<u64>,
    pub memory_size: Option<usize>,
    pub allocator: Option<bool>,
}

struct TaiSyscall {
    registry: Arc<ToolRegistry>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    output_tx: mpsc::Sender<Vec<u8>>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
    ctx: Option<crate::tools::context::ToolContext>,
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

                let request_bytes = machine.memory_mut().load_bytes(req_ptr, req_len)?;

                // Decode postcard frame: [tool_name: postcard String][args: postcard-encoded Args]
                // Use postcard::take_from_bytes to split into tool name + args bytes
                let (tool_name, rest): (String, &[u8]) = postcard::take_from_bytes(&request_bytes)
                    .map_err(|_| VmError::Unexpected("invalid postcard frame: tool name".into()))?;

                let result_bytes = self.registry.execute_postcard(
                    &tool_name,
                    rest,
                    self.x_credentials.as_ref(),
                    self.working_dir.as_deref(),
                    self.ctx.as_ref(),
                );

                let to_write = result_bytes.len().min(out_size as usize);
                if to_write > 0 {
                    machine
                        .memory_mut()
                        .store_bytes(out_ptr, &result_bytes[..to_write])?;
                }
                machine.set_register(registers::A0, to_write as u64);

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
            3 => {
                let req_ptr = machine.registers()[registers::A0];
                let req_len = machine.registers()[registers::A1];
                let out_ptr = machine.registers()[registers::A2];
                let out_size = machine.registers()[registers::A3];

                let request_bytes = machine.memory_mut().load_bytes(req_ptr, req_len)?;

                // Decode batch frame: [count: varint][name: String][args: &[u8]]*
                let (count, mut rest): (u32, &[u8]) = postcard::take_from_bytes(&request_bytes)
                    .map_err(|_| VmError::Unexpected("invalid batch frame: count".into()))?;

                let mut requests: Vec<(String, Vec<u8>)> = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (name, r): (String, &[u8]) = postcard::take_from_bytes(rest)
                        .map_err(|_| VmError::Unexpected("invalid batch frame: name".into()))?;
                    let (args, r): (&[u8], &[u8]) = postcard::take_from_bytes(r)
                        .map_err(|_| VmError::Unexpected("invalid batch frame: args".into()))?;
                    requests.push((name, args.to_vec()));
                    rest = r;
                }

                // Dispatch all tool calls concurrently using std::thread::scope,
                // which ensures every spawned thread completes before we return.
                let registry = &self.registry;
                let xc = self.x_credentials.as_ref();
                let cw = self.working_dir.as_deref();
                let ctx = self.ctx.as_ref();
                let results: Vec<Vec<u8>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = requests
                        .into_iter()
                        .map(|(name, args)| {
                            scope
                                .spawn(move || registry.execute_postcard(&name, &args, xc, cw, ctx))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                // Thread panicked — encode an error result.
                                let err: Result<(), String> =
                                    Err("tool thread panicked".to_string());
                                postcard::to_allocvec(&err).unwrap_or_default()
                            })
                        })
                        .collect()
                });

                // Encode response: [count: varint][result: postcard Result]*.
                // Each result from execute_postcard is already a postcard-encoded
                // Result<Vec<u8>, String>, so we just concatenate them.
                let count_encoded: Vec<u8> = postcard::to_allocvec(&(results.len() as u32))
                    .map_err(|_| VmError::Unexpected("batch encode count failed".into()))?;
                let mut response = count_encoded;
                for r in &results {
                    response.extend_from_slice(r);
                }

                let to_write = response.len().min(out_size as usize);
                if to_write > 0 {
                    machine
                        .memory_mut()
                        .store_bytes(out_ptr, &response[..to_write])?;
                }
                machine.set_register(registers::A0, to_write as u64);
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
            "--edition",
            "2024",
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
        return Err(format!(
            "compilation error:\n{}",
            String::from_utf8_lossy(&stderr_buf)
        ));
    }

    let elf =
        std::fs::read(&output_path).map_err(|e| format!("failed to read compiled program: {e}"))?;

    drop(dir);
    Ok(elf)
}

fn run_riscv_impl(
    input: &RunRiscVInput,
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    write_tx: Option<mpsc::Sender<Vec<u8>>>,
    registry: Arc<ToolRegistry>,
    ctx: Option<crate::tools::context::ToolContext>,
) -> ToolOutput {
    let enable_allocator = input.allocator.unwrap_or(true);

    // Format the user's Rust source with rustfmt before compiling.
    // When rustfmt is unavailable or the file is already well-formatted the
    // output is identical to the input — the fallback is invisible.
    let formatted_source: Option<String> = input.source.as_ref().map(|s| format_rust_source(s));

    // Source to hand to rustc — prefer the formatted version, fall back to
    // the original if formatting failed or was skipped.
    let compile_source: Option<&str> = formatted_source.as_deref().or(input.source.as_deref());

    // Source to include in the result display (also prefer formatted).
    let display_source: Option<&str> = formatted_source.as_deref().or(input.source.as_deref());

    let elf = match (compile_source, input.program.as_deref()) {
        (Some(source), None) => match compile(source, enable_allocator) {
            Ok(elf) => elf,
            Err(e) => {
                return tool_err(e);
            }
        },
        (None, Some(program_b64)) => match BASE64.decode(program_b64) {
            Ok(elf) => elf,
            Err(e) => {
                return tool_err(format!("base64 decode error: {e}"));
            }
        },
        (None, None) => {
            return tool_err("either 'source' or 'program' is required");
        }
        (Some(_), Some(_)) => {
            return tool_err("provide only one of 'source' or 'program'");
        }
    };

    let memory_size = input.memory_size.unwrap_or(4 * 1024 * 1024);
    if !memory_size.is_multiple_of(4096) {
        return tool_err("memory_size must be a multiple of 4096");
    }
    if memory_size > 4 * 1024 * 1024 {
        return tool_err("memory_size cannot exceed 4MB");
    }

    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    let syscall = TaiSyscall {
        registry,
        x_credentials: x_credentials.cloned(),
        working_dir: working_dir.map(|p| p.to_path_buf()),
        output_tx,
        write_tx,
        ctx,
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
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(Bytes::from)
        .collect();

    if let Err(e) = trace.load_program(&Bytes::from(elf), args_list.iter().map(|b| Ok(b.clone()))) {
        return tool_err(format!("failed to load program: {e}"));
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
        Ok(exit_code) => {
            let cycles = trace.machine.cycles();
            // Drop the trace machine first so the TaiSyscall sender is
            // closed before we drain the receiver.  This lets us use a
            // blocking recv() loop that terminates deterministically.
            drop(trace);

            let mut out = Vec::new();
            while let Ok(chunk) = output_rx.recv() {
                out.extend_from_slice(&chunk);
            }
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
            result_content.push_str(&format!(
                "\n[VM: exited with code {exit_code} in {cycles} cycles]"
            ));

            tool_ok(result_content)
        }
        Err(e) => {
            let cycles = trace.machine.cycles();
            // Same reason as above — close the sender before draining.
            drop(trace);

            let mut out = Vec::new();
            while let Ok(chunk) = output_rx.recv() {
                out.extend_from_slice(&chunk);
            }
            let out_str = String::from_utf8_lossy(&out).to_string();
            let msg = if out_str.is_empty() {
                format!("VM error after {cycles} cycles: {e}")
            } else {
                format!("VM error after {cycles} cycles: {e}\noutput so far:\n{out_str}")
            };
            tool_err(msg)
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
    type Args = RunRiscVInput;
    type Return = String;

    fn name(&self) -> &'static str {
        "run_riscv"
    }

    fn group(&self) -> &'static str {
        "vm"
    }

    fn description(&self) -> &'static str {
        "Compile and run Rust code in a RISC-V sandboxed VM. PREFER the 'source' parameter over 'program'. With 'source', only provide a `fn main()` body — the tool auto-generates #![no_std], #![no_main], #[panic_handler], _start, and the `tai` module. Use per-tool convenience wrappers (tai::read_file, tai::write_file, tai::db_get, tai::db_set, tai::sh, tai::exec, tai::grep, tai::find, tai::http_request) for tool calls — they handle the postcard encoding automatically. Use tai::write(b\"...\") for VM output and tai::exit(code) to finish. Do NOT use raw ecall with Linux syscall numbers (64, 93) — they are not supported."
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Rust source code for `fn main()`. CRITICAL: Do NOT include #![no_std], #![no_main], #[panic_handler], _start, or the `tai` module — these are auto-generated. Do NOT use raw ecall with Linux syscall numbers (64 for write, 93 for exit). Use the provided wrappers: tai::write(b\"...\"), tai::exit(code), and per-tool wrappers like tai::read_file(path), tai::write_file(path, content, overwrite), tai::db_get(key), tai::db_set(key, value), tai::sh(command, shell, ...), tai::exec(command, args, ...), tai::grep(pattern), tai::find(pattern), tai::http_request(method, url, headers, body, timeout). The wrappers handle postcard encoding automatically — no need to call tai::tool_call or tai::call directly. Example: `fn main() { let content = tai::read_file(\"Cargo.toml\"); tai::write(content.as_bytes()); }`. When allocator:true (default), alloc types are pre-imported: Vec, String, Box, format!, .to_string()."
                },
                "program": {
                    "type": "string",
                    "description": "Base64-encoded RISC-V ELF binary. Only use if you compiled externally WITH the tai syscall ABI (syscall 0=postcard-encoded tool dispatch, 1=write, 2=exit). Programs using Linux syscall numbers (64=write, 93=exit) will fail. When in doubt, use 'source' instead."
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
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::Other("ToolRegistry no longer available".to_string()))?;
        let output = run_riscv_impl(
            &args,
            x_credentials,
            working_dir,
            None,
            registry,
            ctx.cloned(),
        );
        if output.is_error {
            Err(ToolError::Other(output.content))
        } else {
            Ok(output.content)
        }
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolError::Other("ToolRegistry no longer available".to_string()))?;
        let output = run_riscv_impl(
            &args,
            x_credentials,
            working_dir,
            Some(output_tx),
            registry,
            ctx.cloned(),
        );
        if output.is_error {
            Err(ToolError::Other(output.content))
        } else {
            Ok(output.content)
        }
    }
}

pub fn execute_run_riscv_tool(
    input: &RunRiscVInput,
    working_dir: Option<&Path>,
) -> Result<String, ToolError> {
    let registry = Arc::new(ToolRegistry::new());
    let output = run_riscv_impl(input, None, working_dir, None, registry, None);
    if output.is_error {
        Err(ToolError::Other(output.content))
    } else {
        Ok(output.content)
    }
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
        assert!(
            result.contains("struct BumpAlloc"),
            "should contain bump allocator"
        );
        assert!(result.contains("fn args()"), "should contain args()");
        assert!(result.contains("fn _start()"), "should contain _start");
        assert!(
            result.contains("tai::exit(1)"),
            "should contain panic handler"
        );
    }

    #[test]
    fn build_boilerplate_without_alloc_excludes_allocator() {
        let result = build_boilerplate(false);
        assert!(
            !result.contains("struct BumpAlloc"),
            "should NOT contain bump allocator"
        );
        assert!(!result.contains("fn args()"), "should NOT contain args()");
        assert!(
            result.contains("fn _start()"),
            "should still contain _start"
        );
        assert!(
            result.contains("tai::exit(1)"),
            "should still contain panic handler"
        );
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
    fn run_riscv_requires_source_or_program() {
        let result = run_riscv_impl(
            &RunRiscVInput::default(),
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            result.content.contains("source") || result.content.contains("program"),
            "should mention source/program: {}",
            result.content
        );
    }

    #[test]
    fn run_riscv_rejects_both_source_and_program() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                source: Some("fn main() {}".to_string()),
                program: Some("AAAA".to_string()),
                ..Default::default()
            },
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(result.content.contains("only one of"), "{}", result.content);
    }

    #[test]
    fn run_riscv_rejects_invalid_base64() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("!!!not-base64!!!".to_string()),
                ..Default::default()
            },
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            result.content.contains("base64 decode error"),
            "{}",
            result.content
        );
    }

    #[test]
    fn run_riscv_rejects_non_aligned_memory() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                memory_size: Some(100),
                ..Default::default()
            },
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            result.content.contains("multiple of 4096"),
            "{}",
            result.content
        );
    }

    #[test]
    fn run_riscv_rejects_memory_over_4mb() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                memory_size: Some(4198400),
                ..Default::default()
            },
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            result.content.contains("cannot exceed 4MB"),
            "{}",
            result.content
        );
    }

    #[test]
    fn run_riscv_accepts_valid_base64_program_with_4k_aligned_memory() {
        // "AAAA" decodes to 3 zero bytes — not a valid ELF, but that's caught at load time,
        // not during input validation. This test verifies that valid base64 + aligned memory
        // passes input validation (the error will be about ELF loading, not input).
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                memory_size: Some(4096),
                ..Default::default()
            },
            None,
            None,
            None,
            dummy_registry(),
            None,
        );
        // Should fail at ELF load, not at input validation
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            !result.content.contains("base64 decode error"),
            "should not be base64 error: {}",
            result.content
        );
        assert!(
            !result.content.contains("multiple of 4096"),
            "should not be alignment error: {}",
            result.content
        );
        assert!(
            !result.content.contains("cannot exceed 4MB"),
            "should not be size error: {}",
            result.content
        );
    }

    // -- Channel drain tests ----------------------------------------------
    //
    // These verify the blocking `recv()` drain pattern used in the
    // production path: drop the sender first (simulating `drop(trace)`
    // from run_riscv_impl), then recv() until Disconnected.

    #[test]
    fn channel_drain_empty_when_nothing_sent() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        drop(tx);

        let mut out = Vec::new();
        while let Ok(chunk) = rx.recv() {
            out.extend_from_slice(&chunk);
        }
        assert!(out.is_empty());
    }

    #[test]
    fn channel_drain_single_chunk() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hello".to_vec()).unwrap();
        drop(tx);

        let mut out = Vec::new();
        while let Ok(chunk) = rx.recv() {
            out.extend_from_slice(&chunk);
        }
        assert_eq!(out, b"hello");
    }

    #[test]
    fn channel_drain_preserves_order() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(b"world".to_vec()).unwrap();
        drop(tx);

        let mut out = Vec::new();
        while let Ok(chunk) = rx.recv() {
            out.extend_from_slice(&chunk);
        }
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn channel_drain_multiple_chunks_closed_sender() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"a".to_vec()).unwrap();
        tx.send(b"b".to_vec()).unwrap();
        tx.send(b"c".to_vec()).unwrap();
        drop(tx);

        let mut out = Vec::new();
        while let Ok(chunk) = rx.recv() {
            out.extend_from_slice(&chunk);
        }
        assert_eq!(out, b"abc");
    }

    // ── Batch ecall tests ─────────────────────────────────────────

    /// A minimal tool that returns a constant string — used to verify
    /// concurrent dispatch of multiple tool calls. Uses `()` as args so
    /// postcard always decodes cleanly (single-byte unit).
    struct EchoTestTool {
        name: &'static str,
        response: &'static str,
    }

    impl Tool for EchoTestTool {
        type Args = ();
        type Return = String;

        fn name(&self) -> &'static str {
            self.name
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool for concurrent dispatch"
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            Ok(self.response.to_string())
        }
    }

    #[test]
    fn echo_test_tool_works_with_execute_postcard() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTestTool {
            name: "_echo_solo",
            response: "hello",
        });
        let registry = registry.build();

        // Postcard-encoded () unit — EchoTestTool takes no args.
        let unit_args: Vec<u8> = postcard::to_allocvec(&()).unwrap();
        let result = registry.execute_postcard("_echo_solo", &unit_args, None, None, None);

        assert!(!result.is_empty(), "should have a result");
        if result[0] == 0 {
            // Decode the Ok payload.
            let (payload, _rest): (Vec<u8>, &[u8]) =
                postcard::take_from_bytes(&result[1..]).unwrap();
            assert_eq!(payload, b"hello", "payload mismatch");
        } else {
            // Decode the error message for debugging.
            let (err_msg, _rest): (String, &[u8]) =
                postcard::take_from_bytes(&result[1..]).unwrap();
            panic!("execute_postcard returned Err: {err_msg}");
        }
    }

    #[test]
    fn concurrent_tool_dispatch_via_thread_scope() {
        // Register two tools with different names, then dispatch both
        // concurrently via thread::scope — the same pattern used in
        // the batch ecall (ecall 3). Verify both results come back.
        let mut registry = ToolRegistry::new();
        registry.register(EchoTestTool {
            name: "_echo_a",
            response: "result_a",
        });
        registry.register(EchoTestTool {
            name: "_echo_b",
            response: "result_b",
        });
        let registry = registry.build();

        // Postcard-encoded () unit — EchoTestTool takes no args.
        let unit_args: Vec<u8> = postcard::to_allocvec(&()).unwrap();
        let requests = vec![
            ("_echo_a".to_string(), unit_args.clone()),
            ("_echo_b".to_string(), unit_args),
        ];

        let results: Vec<Vec<u8>> = std::thread::scope(|scope| {
            let handles: Vec<_> = requests
                .into_iter()
                .map(|(name, args)| {
                    let reg = Arc::clone(&registry);
                    scope.spawn(move || reg.execute_postcard(&name, &args, None, None, None))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });

        assert_eq!(results.len(), 2, "should have 2 results");

        // Each result is a postcard-encoded Result<Vec<u8>, String>.
        // For Ok values, the encoding is: 0x00 (Ok tag) + postcard(Vec<u8>).
        for (i, result) in results.iter().enumerate() {
            assert!(
                !result.is_empty() && result[0] == 0,
                "result {} should be Ok, got tag {:?}",
                i,
                result.first(),
            );
        }

        // Decode payloads to verify they match expected responses.
        fn decode_ok_payload(data: &[u8]) -> Vec<u8> {
            assert_eq!(data[0], 0, "expected Ok tag byte");
            let (payload, _rest): (Vec<u8>, &[u8]) = postcard::take_from_bytes(&data[1..]).unwrap();
            payload
        }

        assert_eq!(
            decode_ok_payload(&results[0]),
            b"result_a",
            "first result content mismatch",
        );
        assert_eq!(
            decode_ok_payload(&results[1]),
            b"result_b",
            "second result content mismatch",
        );
    }
}
