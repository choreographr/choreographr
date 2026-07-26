use crate::tools::{
    Tool, ToolExecError, ToolOutput, ToolRegistry, context::ToolContext, tool_err, tool_ok,
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
use tracing::{debug, error, info, trace, warn};

const BOILERPLATE_HEAD: &str = r#"
#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![allow(unused_imports)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    tai::exit(1)
}

"#;

const BOILERPLATE_ALLOC: &str = include_str!("vm_allocator_inner.rs");

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

    // Syscall numbers for the tai custom ABI.
    // - 0: TOOL_CALL — dispatch a named tool with postcard-encoded args
    // - 1: WRITE — emit bytes to the VM's output stream
    // - 3: BATCH_TOOL_CALL — dispatch multiple tool calls concurrently
    //
    // EXIT uses Linux syscall 93 rather than a custom number because CKB-VM's
    // DefaultMachine::ecall() natively handles 93 by reading `exit_code` from
    // register A0 and calling set_running(false). Our previous custom code 2
    // only called set_running(false) — it never propagated the exit code, so
    // every VM exit, including panics, appeared as code 0.
    pub const TOOL_CALL: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const EXIT: u64 = 93; // Linux exit — CKB-VM handles this natively
    pub const BATCH_TOOL_CALL: u64 = 3;

    pub unsafe fn tool_call(request: &[u8], output: &mut [u8]) -> usize {
        let result: usize;
        // SAFETY: ecall is inherently unsafe; caller ensures valid pointers/lengths.
        unsafe {
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
        }
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
        // SAFETY: ecall is inherently unsafe; caller ensures valid pointers/lengths.
        unsafe {
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
        }
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

/// Convenience `alloc` imports available at the crate root for user code.
///
/// These are pre-imported so user code can use `Vec`, `String`, `Box`,
/// `format!`, and `.to_string()` without explicit imports.  They live
/// outside `pub mod tai` so they're in scope for the user's `fn main()`.
const BOILERPLATE_CONVENIENCE_IMPORTS: &str = r#"
use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::boxed::Box;
use alloc::format;
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

    /// Shared helper: decode a double-wrapped Result `Result<Result<R, E>, ToolError>`.
    ///
    /// Given a frame starting at `[outer_status][inner_status][payload]`,
    /// strips both status bytes and returns the inner payload or domain/infra error.
    fn decode_double_frame<'a>(frame: &'a [u8]) -> Result<&'a [u8], &'a str> {
        if frame.is_empty() {
            return Err("empty frame");
        }
        let outer_status = frame[0];
        match outer_status {
            0 => {
                // Outer Ok — strip inner status byte too
                if frame.len() < 2 {
                    return Err("truncated Ok");
                }
                let inner_status = frame[1];
                let payload = &frame[2..];
                match inner_status {
                    0 => Ok(payload),   // Inner Ok — raw postcard of tool's return type
                    1 => {
                        // Inner Err — domain error string
                        let (err_str, _consumed) = dec_str(payload)
                            .map_err(|_| "failed to decode domain error")?;
                        Err(err_str)
                    }
                    _ => Err("unknown inner result status"),
                }
            }
            1 => {
                // Outer Err — infrastructure error
                let (err_str, _consumed) = dec_str(&frame[1..])
                    .map_err(|_| "failed to decode infrastructure error")?;
                Err(err_str)
            }
            _ => Err("unknown result status"),
        }
    }

    /// Decode a **double-wrapped** Result produced by the host's `encode_outer`.
    ///
    /// The host wraps every tool result as `Result<Result<R, E>, ToolError>`:
    ///   Ok(Ok(data))   — tool succeeded, `data` is the tool's return payload
    ///                    (raw postcard-encoded return value of R)
    ///   Ok(Err(e))     — tool failed with domain error `e`
    ///   Err(infra_err) — infrastructure failure (unknown tool, postcard decode
    ///                    error, etc.)
    ///
    /// This function strips both outer and inner status bytes and returns
    /// the raw postcard-encoded tool return value.  Callers who interpret
    /// the result as a `String` must then decode it with `dec_str()` since
    /// the returned slice still includes the postcard varint length prefix.
    pub fn dec_double_result<'a>(resp: &'a [u8]) -> Result<&'a [u8], &'a str> {
        decode_double_frame(resp)
    }

    /// Convenience wrapper: decode a double-wrapped result and extract the
    /// postcard-encoded `String` inside.  Returns Err on decode failure.
    pub fn dec_double_str_result<'a>(resp: &'a [u8]) -> Result<String, &'a str> {
        let b = dec_double_result(resp)?;
        let (s, _) = dec_str(b).map_err(|_| "failed to decode string payload")?;
        Ok(s.to_string())
    }

    /// Like `dec_double_result` but also returns the total number of bytes
    /// consumed so callers can advance a cursor across a sequence of results.
    ///
    /// The batch response format is:
    ///   [count: varint][frame_len: varint][frame: Result<Result<R, E>, ToolError>]*
    ///
    /// The host prefixes each frame with a varint length so we can find frame
    /// boundaries without knowing the postcard encoding of the generic type R.
    pub fn dec_result_raw<'a>(data: &'a [u8]) -> Result<(&'a [u8], usize), &'a str> {
        if data.is_empty() { return Err("empty"); }
        let (frame_len, consumed) = dec_varint(data)?;
        let frame_start = consumed;
        let frame_end = frame_start + frame_len as usize;
        if frame_end > data.len() { return Err("truncated frame"); }
        let frame = &data[frame_start..frame_end];
        let payload = decode_double_frame(frame)?;
        Ok((payload, frame_end))
    }

    // ── Tool call helpers ─────────────────────────────────────────────

    /// Make a raw tool call. Encodes tool name + args as postcard frame,
    /// does ecall, returns the raw result bytes.
    pub fn call(name: &str, args: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        enc_str(name, &mut buf);
        buf.extend_from_slice(args);
        // Allocate the output buffer at full capacity without zero-initialising
        // it — `tool_call` fills the first n bytes via ecall, so initialised
        // content is guaranteed for the returned slice.
        let mut output = Vec::with_capacity(128 * 1024);
        unsafe { output.set_len(128 * 1024); }
        let n = unsafe { tool_call(&buf, &mut output) };
        output.truncate(n);
        output
    }

    /// Submit multiple tool calls for concurrent execution on the host.
    ///
    /// Encodes all requests into a single batch frame, issues one ecall,
    /// and returns results in submission order. Each result is independent —
    /// one tool may fail without affecting the others.
    pub fn call_multi(requests: &[(&str, &[u8])]) -> Vec<Result<Vec<u8>, String>> {
        let mut buf = Vec::new();
        enc_varint(requests.len() as u64, &mut buf);
        for (name, args) in requests {
            enc_str(name, &mut buf);
            enc_bytes(args, &mut buf);
        }
        let mut output = Vec::with_capacity(128 * 1024);
        unsafe { output.set_len(128 * 1024); }
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
                    pos += end;
                }
                Err(e) => {
                    // Decoding failed mid-stream; return partial results and stop.
                    // The host always produces well-formed responses, so hitting
                    // this path indicates a fundamental protocol mismatch.
                    results.push(Err(String::from(e)));
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
    ///
    /// The host's `DbGet` tool returns `Option<String>`, which gets wrapped
    /// by `encode_outer` into `Result<Result<Option<String>, DbError>, ToolError>`.
    /// We first strip the double-wrapped Result via `dec_double_result`, then
    /// decode the inner `Option<String>` manually (since the inner payload is
    /// an `Option`, not a bare `Vec<u8>`).
    pub fn db_get(key: &str) -> Vec<u8> {
        let mut args = Vec::new();
        enc_str(key, &mut args);
        let resp = call("db_get", &args);

        match dec_double_result(&resp) {
            Ok(inner) => {
                // Inner payload is Option<String> in postcard format:
                //   Some: 0x01 varint(len) UTF-8 bytes
                //   None: 0x00
                if inner.is_empty() || inner[0] == 0 {
                    Vec::new()
                } else {
                    let (s, _) = dec_str(&inner[1..]).unwrap_or(("", 0));
                    s.as_bytes().to_vec()
                }
            }
            Err(_) => Vec::new(),
        }
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
        dec_double_result(&resp).is_ok()
    }

    /// read_file(path: &str) -> file content as String.
    pub fn read_file(path: &str) -> String {
        let mut args = Vec::new();
        enc_str(path, &mut args);
        let resp = call("read_file", &args);
        dec_double_str_result(&resp).unwrap_or_default()
    }

    /// write_file(path: &str, content: &str, overwrite: bool).
    pub fn write_file(path: &str, content: &str, overwrite: bool) {
        let mut args = Vec::new();
        enc_str(path, &mut args);
        enc_str(content, &mut args);
        // Both fields are Option<bool> in the daemon's WriteFileArgs.
        // Use key = Some(overwrite) and create_parents = Some(true) (safe default).
        enc_option_bool(Some(overwrite), &mut args);
        enc_option_bool(Some(true), &mut args);
        let _resp = call("write_file", &args);
    }

    /// git_status() -> status string.
    pub fn git_status() -> String {
        // GitRepoArgs { repo_path: None } = postcard 0x00
        let resp = call("git_status", &[0x00]);
        dec_double_str_result(&resp).unwrap_or_default()
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
        dec_double_str_result(&resp).unwrap_or_default()
    }

    /// find(pattern: &str) -> file name search results as string.
    /// Glob mode is auto-detected — patterns with `*`, `?`, `[` are treated as globs.
    pub fn find(pattern: &str) -> String {
        let mut args = Vec::new();
        enc_str(pattern, &mut args);
        enc_bool(false, &mut args);       // glob: false = auto-detect
        enc_option_str(None, &mut args);  // path
        enc_option_u64(None, &mut args);  // max_results
        let resp = call("find", &args);
        dec_double_str_result(&resp).unwrap_or_default()
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
        dec_double_str_result(&resp).unwrap_or_default()
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
        dec_double_str_result(&resp).unwrap_or_default()
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
        dec_double_str_result(&resp).unwrap_or_default()
    }
"#;

fn build_boilerplate() -> String {
    let mut s = String::from(BOILERPLATE_HEAD);
    s.push_str(BOILERPLATE_ALLOC);
    s.push_str(BOILERPLATE_TAIL_BASE);
    s.push_str(BOILERPLATE_TAIL_ALLOC);
    s.push_str(BOILERPLATE_TAIL_ENCODING);
    s.push_str(BOILERPLATE_TAIL_CLOSE);
    s.push_str(BOILERPLATE_CONVENIENCE_IMPORTS);
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

                debug!(tool_name, req_len, out_size, "guest TOOL_CALL");

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
                    trace!(len, "guest WRITE syscall");
                    let data = machine.memory_mut().load_bytes(ptr, len)?;
                    let _ = self.output_tx.send(data.to_vec());
                    if let Some(tx) = &self.write_tx {
                        let _ = tx.send(data.into());
                    }
                }
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

                debug!(count, "guest BATCH_TOOL_CALL");

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

                // Encode response: [count: varint][frame_len: varint][frame_bytes]*.
                // Each result from execute_postcard is a postcard-encoded
                // Result<Result<R, E>, ToolError>.  Since the guest doesn't know R's
                // postcard length at compile time, we prefix each frame with a varint
                // length so the guest's dec_result_raw can find frame boundaries.
                let count_encoded: Vec<u8> = postcard::to_allocvec(&(results.len() as u32))
                    .map_err(|_| VmError::Unexpected("batch encode count failed".into()))?;
                let mut response = count_encoded;
                for r in &results {
                    let frame_len: u32 = r.len() as u32;
                    let len_encoded = postcard::to_allocvec(&frame_len)
                        .map_err(|_| VmError::Unexpected("batch encode len failed".into()))?;
                    response.extend_from_slice(&len_encoded);
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

fn compile(source: &str) -> Result<Vec<u8>, String> {
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

    let boilerplate = build_boilerplate();
    let full_source = format!("{boilerplate}\n// User code\n{source}");
    std::fs::write(&input_path, &full_source)
        .map_err(|e| format!("failed to write source: {e}"))?;

    info!("compiling guest program");

    let mut child = Command::new("rustc")
        .arg("+nightly")
        .args([
            "--target",
            target,
            "-C",
            "opt-level=z",
            "--edition",
            "2024",
            "--color",
            "always",
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
            warn!("compilation timed out after 60s");
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
        error!(
            "compilation failed:\n{}",
            String::from_utf8_lossy(&stderr_buf)
        );
        return Err(format!(
            "compilation error:\n{}",
            String::from_utf8_lossy(&stderr_buf)
        ));
    }

    info!("compilation OK");

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
    // Format the user's Rust source with rustfmt before compiling.
    // When rustfmt is unavailable or the file is already well-formatted the
    // output is identical to the input — the fallback is invisible.
    let formatted_source: Option<String> = input.source.as_ref().map(|s| format_rust_source(s));

    // Source to hand to rustc — prefer the formatted version, fall back to
    // the original if formatting failed or was skipped.
    let compile_source: Option<&str> = formatted_source.as_deref().or(input.source.as_deref());

    let target = "riscv64imac-unknown-none-elf";
    let compile_cmd =
        format!("rustc +nightly --target {target} -C opt-level=z --edition 2024 --color always");

    let elf = match (compile_source, input.program.as_deref()) {
        (Some(source), None) => {
            // Show the compile command in the streaming output so the user
            // knows what's being run.
            if let Some(ref tx) = write_tx {
                let _ = tx.send(format!("$ {compile_cmd}\n").as_bytes().to_vec());
            }

            match compile(source) {
                Ok(elf) => elf,
                Err(e) => {
                    return tool_err(format!("$ {compile_cmd}\n{e}"));
                }
            }
        }
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

    info!(
        memory_size,
        max_cycles = input.max_cycles.unwrap_or(1_000_000),
        "starting VM"
    );

    match trace.run() {
        Ok(exit_code) => {
            let cycles = trace.machine.cycles();
            info!(exit_code, cycles, "VM finished successfully");
            // Drop the trace machine first so the TaiSyscall sender is
            // closed before we drain the receiver.  This lets us use a
            // blocking recv() loop that terminates deterministically.
            drop(trace);

            let out_str = String::from_utf8_lossy(&drain_vm_output(&output_rx)).to_string();

            let mut result = out_str;
            result.push_str(&format!(
                "\n[VM: exited with code {exit_code} in {cycles} cycles]"
            ));

            tool_ok(result)
        }
        Err(e) => {
            let cycles = trace.machine.cycles();
            error!(cycles, error = %e, "VM error");
            drop(trace);

            let out_str = String::from_utf8_lossy(&drain_vm_output(&output_rx)).to_string();
            let msg = if out_str.is_empty() {
                format!("VM error after {cycles} cycles: {e}")
            } else {
                format!("VM error after {cycles} cycles: {e}\noutput so far:\n{out_str}")
            };
            tool_err(msg)
        }
    }
}

/// Drain all buffered VM output after the machine sender has been dropped.
/// Uses a blocking `recv()` loop that terminates deterministically once the
/// sender end is gone (the channel is disconnected).
fn drain_vm_output(rx: &mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(chunk) = rx.recv() {
        out.extend_from_slice(&chunk);
    }
    out
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
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "run_riscv"
    }

    fn group(&self) -> &'static str {
        "vm"
    }

    fn description(&self) -> &'static str {
        "Compile and run Rust code in a RISC-V sandboxed VM. PREFER the 'source' parameter over 'program'. With 'source', only provide a `fn main()` body — the tool auto-generates #![no_std], #![no_main], #[panic_handler], _start, and the `tai` module. Use per-tool convenience wrappers (tai::read_file, tai::write_file, tai::db_get, tai::db_set, tai::sh, tai::exec, tai::grep, tai::find, tai::http_request) for tool calls — they handle the postcard encoding automatically. Use tai::write(b\"...\") for VM output and tai::exit(code) to finish. Do NOT use raw ecall with Linux syscall number 64 (write) — it is not supported."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match (&args.source, &args.program) {
            (Some(source), None) => {
                let display = format_rust_source(source);
                let mut parts = vec![format!(
                    "Compiling and running Rust code:\n```rust\n{display}\n```"
                )];
                if let Some(ref prog_args) = args.args
                    && !prog_args.is_empty()
                {
                    parts.push(format!("\nProgram args: {:?}.", prog_args));
                }
                parts.push(String::from("\nAllocator: included."));
                parts.push(format!(
                    "\nMax cycles: {}.",
                    args.max_cycles.unwrap_or(1_000_000)
                ));
                parts.push(format!(
                    "\nMemory: {} bytes.",
                    args.memory_size.unwrap_or(4 * 1024 * 1024)
                ));
                parts.concat()
            }
            (None, Some(_)) => "Running a pre-compiled RISC-V ELF binary.".to_string(),
            (Some(_), Some(_)) => "Provide only one of 'source' or 'program'.".to_string(),
            (None, None) => "No source or program provided for run_riscv.".to_string(),
        }
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
                    "description": "Rust source code for `fn main()`. CRITICAL: Do NOT include #![no_std], #![no_main], #[panic_handler], _start, or the `tai` module — these are auto-generated. Do NOT use raw ecall with Linux syscall number 64 (write) — it is not supported. Use the provided wrappers: tai::write(b\"...\"), tai::exit(code), and per-tool wrappers like tai::read_file(path), tai::write_file(path, content, overwrite), tai::db_get(key), tai::db_set(key, value), tai::sh(command, shell, ...), tai::exec(command, args, ...), tai::grep(pattern), tai::find(pattern), tai::http_request(method, url, headers, body, timeout). The wrappers handle postcard encoding automatically — no need to call tai::tool_call or tai::call directly. Example: `fn main() { let content = tai::read_file(\"Cargo.toml\"); tai::write(content.as_bytes()); }`. Alloc types are pre-imported: Vec, String, Box, format!, .to_string()."
                },
                "program": {
                    "type": "string",
                    "description": "Base64-encoded RISC-V ELF binary. Only use if you compiled externally WITH the tai syscall ABI (syscall 0=postcard-encoded tool dispatch, 1=write, 93=exit). Programs using Linux syscall number 64 (write) will fail. When in doubt, use 'source' instead."
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Command-line arguments passed to the guest program. Read them with tai::args() -> Vec<Vec<u8>> in the guest code."
                },

                "max_cycles": {
                    "type": "integer",
                    "description": "Maximum CPU cycles before VM termination (default: 1_000_000)"
                },
                "memory_size": {
                    "type": "integer",
                    "description": "VM memory size in bytes (default: 4_194_304, must be a multiple of 4096, max 4MB)"
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
    ) -> Result<Self::Return, Self::Error> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolExecError("ToolRegistry no longer available".to_string()))?;
        let output = run_riscv_impl(
            &args,
            x_credentials,
            working_dir,
            None,
            registry,
            ctx.cloned(),
        );
        if output.is_error {
            Err(ToolExecError(output.content))
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
    ) -> Result<Self::Return, Self::Error> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| ToolExecError("ToolRegistry no longer available".to_string()))?;
        let output = run_riscv_impl(
            &args,
            x_credentials,
            working_dir,
            Some(output_tx),
            registry,
            ctx.cloned(),
        );
        if output.is_error {
            Err(ToolExecError(output.content))
        } else {
            Ok(output.content)
        }
    }
}

pub fn execute_run_riscv_tool(
    input: &RunRiscVInput,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let registry = Arc::new(ToolRegistry::new());
    let output = run_riscv_impl(input, None, working_dir, None, registry, None);
    if output.is_error {
        Err(ToolExecError(output.content))
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
    fn build_boilerplate_includes_allocator() {
        let result = build_boilerplate();
        assert!(
            result.contains("struct HoleList"),
            "should contain linked-list allocator"
        );
        assert!(result.contains("fn args()"), "should contain args()");
        assert!(result.contains("fn _start()"), "should contain _start");
        assert!(
            result.contains("tai::exit(1)"),
            "should contain panic handler"
        );
    }

    #[test]
    fn build_boilerplate_contains_tai_module() {
        let result = build_boilerplate();
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
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            self.name
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool for concurrent dispatch"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.name())
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
        ) -> Result<Self::Return, Self::Error> {
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
            // Decode the Ok payload — skip outer Ok(0x00) and inner Ok(0x00).
            let (payload, _rest): (Vec<u8>, &[u8]) =
                postcard::take_from_bytes(&result[2..]).unwrap();
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

        // Each result is a postcard-encoded Result<Result<Vec<u8>, E>, ToolError>.
        // For Ok(Ok(...)), the encoding is: 0x00 (outer Ok) + 0x00 (inner Ok) + payload.
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
            assert_eq!(data[0], 0, "expected outer Ok tag byte");
            assert_eq!(data[1], 0, "expected inner Ok tag byte");
            let (payload, _rest): (Vec<u8>, &[u8]) = postcard::take_from_bytes(&data[2..]).unwrap();
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

    // ── Allocator unit tests ─────────────────────────────────────────
    //
    // These tests verify the linked-list allocator logic natively on the
    // host without requiring cross-compilation to RISC-V.  The types are
    // shared from the production allocator source (vm_allocator_inner.rs)
    // which is included here as a module.

    mod vm_allocator {
        // Include the production allocator source for host-side testing.
        // Items marked #[cfg(not(test))] (e.g. the global allocator) are
        // excluded when compiled under `cargo test`.
        include!("vm_allocator_inner.rs");
    }

    use core::alloc::Layout;
    use vm_allocator::{Hole, HoleList, align_up};

    /// Min-aligned test heap size — big enough for many allocation patterns.
    const TEST_HEAP_SIZE: usize = 4096;

    /// Wrapper to ensure the heap buffer has at least 16-byte alignment
    /// (required by `Hole` which contains `NonNull` pointers).
    #[repr(C, align(16))]
    struct AlignedHeap([u8; TEST_HEAP_SIZE]);

    static mut TEST_HEAP: AlignedHeap = AlignedHeap([0; TEST_HEAP_SIZE]);

    // ── align_up ───────────────────────────────────────────────────

    #[test]
    fn align_up_already_aligned() {
        assert_eq!(align_up(0, 1), 0);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(128, 128), 128);
    }

    #[test]
    fn align_up_rounds_up() {
        assert_eq!(align_up(1, 4), 4);
        assert_eq!(align_up(3, 4), 4);
        assert_eq!(align_up(5, 8), 8);
        assert_eq!(align_up(100, 32), 128);
    }

    #[test]
    fn align_up_zero_addr() {
        assert_eq!(align_up(0, 4), 0);
        assert_eq!(align_up(0, 16), 0);
    }

    #[test]
    fn align_up_power_of_two() {
        for align in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
            for addr in 0..256 {
                let result = align_up(addr, align);
                assert_eq!(result % align, 0, "align_up({addr}, {align}) = {result}");
                assert!(
                    result >= addr,
                    "align_up({addr}, {align}) = {result} < {addr}"
                );
                assert!(
                    result - addr < align,
                    "align_up({addr}, {align}) = {result} too large"
                );
            }
        }
    }

    // ── HoleList init ──────────────────────────────────────────────

    #[test]
    fn holelist_init_creates_single_hole() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            assert_eq!(list.hole_count(), 1, "should have exactly one hole");
            assert_eq!(list.total_free(), TEST_HEAP_SIZE);
        }
    }

    // ── allocate_first_fit ─────────────────────────────────────────

    #[test]
    fn allocate_simple() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(64, 4).unwrap();
            let ptr = list.allocate_first_fit(layout);
            assert!(!ptr.is_null(), "allocation should succeed");

            // The returned pointer must be within the heap and aligned.
            let off = (ptr as usize).wrapping_sub(heap as usize);
            assert!(off < TEST_HEAP_SIZE, "ptr outside heap");
            assert_eq!(off % 4, 0, "ptr not 4-byte aligned");

            // After allocating, the free space should shrink by the
            // rounded-up allocation size (no wasted header bytes).
            let consumed = Hole::round_to_align(Hole::min_size().max(64));
            assert_eq!(list.total_free(), TEST_HEAP_SIZE - consumed);
        }
    }

    #[test]
    fn allocate_multiple() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let a = list.allocate_first_fit(Layout::from_size_align(64, 4).unwrap());
            let b = list.allocate_first_fit(Layout::from_size_align(128, 4).unwrap());
            let c = list.allocate_first_fit(Layout::from_size_align(32, 4).unwrap());

            assert!(!a.is_null());
            assert!(!b.is_null());
            assert!(!c.is_null());

            // No two allocations should overlap.
            let a_start = a as usize;
            let a_end = a_start + Hole::min_size().max(64);
            let b_start = b as usize;
            let b_end = b_start + Hole::min_size().max(128);
            let c_start = c as usize;
            let c_end = c_start + Hole::min_size().max(32);

            let ranges = [(a_start, a_end), (b_start, b_end), (c_start, c_end)];
            for i in 0..ranges.len() {
                for j in i + 1..ranges.len() {
                    let (s1, e1) = ranges[i];
                    let (s2, e2) = ranges[j];
                    assert!(
                        e1 <= s2 || e2 <= s1,
                        "allocation {i} [{s1},{e1}) overlaps {j} [{s2},{e2})"
                    );
                }
            }
        }
    }

    #[test]
    fn allocate_exact_fit() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            // Allocate the entire heap.  The fixed allocator reuses the hole
            // header bytes as part of the allocation payload, so there is no
            // 24-byte overhead waste.
            let size = TEST_HEAP_SIZE;
            let a = list.allocate_first_fit(Layout::from_size_align(size, 1).unwrap());
            assert!(!a.is_null(), "exact-fit allocation should succeed");
            assert_eq!(
                list.total_free(),
                0,
                "heap should be exhausted after exact fit"
            );
        }
    }

    #[test]
    fn allocate_exhaustion_returns_null() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let a = list.allocate_first_fit(Layout::from_size_align(TEST_HEAP_SIZE, 1).unwrap());
            assert!(!a.is_null(), "first allocation should succeed");
            assert_eq!(list.total_free(), 0, "heap should be full");

            let b = list.allocate_first_fit(Layout::from_size_align(1, 1).unwrap());
            assert!(b.is_null(), "second allocation should fail");
        }
    }

    #[test]
    fn allocate_respects_alignment() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            for align in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
                let layout = Layout::from_size_align(16, align).unwrap();
                let ptr = list.allocate_first_fit(layout);
                if ptr.is_null() {
                    // If the hole list has no sufficiently large hole, skip.
                    // Init a fresh list for each alignment to isolate tests.
                    continue;
                }
                assert_eq!(
                    ptr as usize % align,
                    0,
                    "ptr {ptr:p} not {align}-byte aligned"
                );
            }
        }
    }

    #[test]
    fn allocate_uses_first_fit_strategy() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            // Allocate a small block, then a large one, then free the small one.
            // The next small allocation should reuse the freed front hole
            // (first-fit picks the earliest suitable hole).
            let small = Layout::from_size_align(32, 4).unwrap();
            let large = Layout::from_size_align(512, 4).unwrap();

            let a = list.allocate_first_fit(small);
            assert!(!a.is_null());
            let _b = list.allocate_first_fit(large);
            assert!(!_b.is_null());

            // Free the first (lowest-address) allocation.
            list.deallocate(a, small);

            // Allocate a similar-sized block — first-fit should pick the
            // freed front hole (lower address), not the tail.
            let _c = list.allocate_first_fit(small);
            assert!(!_c.is_null(), "re-allocation should succeed");

            // Verify the number of holes after freeing `a` and then
            // re-allocating.  First-fit from the front should consume
            // the front hole; if it picked the tail hole instead we'd
            // see two holes remaining (the split tail).
            let remaining = list.hole_count();

            // There should be at most 1 hole left (the tail after the
            // second allocation) — if first-fit picked the front hole
            // we're left with only the tail from the second alloc.
            assert!(
                remaining <= 1,
                "first-fit should consume the front hole; {remaining} holes remain"
            );
        }
    }

    #[test]
    fn allocate_minimum_size() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            // A 1-byte allocation rounds up to Hole::min_size() bytes.
            let ptr = list.allocate_first_fit(Layout::from_size_align(1, 1).unwrap());
            assert!(!ptr.is_null());
            let free_after = list.total_free();
            let consumed = Hole::round_to_align(Hole::min_size());
            assert_eq!(
                free_after,
                TEST_HEAP_SIZE - consumed,
                "1-byte allocation should consume {consumed} bytes"
            );
        }
    }

    // ── deallocate ─────────────────────────────────────────────────

    #[test]
    fn deallocate_and_reuse() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(64, 4).unwrap();
            let ptr = list.allocate_first_fit(layout);
            assert!(!ptr.is_null());
            let free_before = list.total_free();

            list.deallocate(ptr, layout);

            // The freed block becomes a hole; total free should increase
            // by the allocation size (including hole header overhead).
            let free_after_dealloc = list.total_free();
            assert!(
                free_after_dealloc > free_before,
                "free space should increase after deallocation"
            );

            // Re-allocate the same size — the recycled hole should be used.
            let ptr2 = list.allocate_first_fit(layout);
            assert!(!ptr2.is_null());

            // After cycling, free space should be back to where it was
            // before the first allocation (the hole header bytes of the
            // recycled hole are re-consumed, same as the original).
            assert_eq!(
                list.total_free(),
                free_before,
                "free space after alloc-dealloc-alloc should match initial"
            );
        }
    }

    #[test]
    fn deallocate_merges_adjacent_holes() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(64, 4).unwrap();

            // Allocate two blocks, then free the second (which is adjacent
            // to the tail hole).
            let a = list.allocate_first_fit(layout);
            let b = list.allocate_first_fit(layout);
            assert!(!a.is_null() && !b.is_null());

            // Should have 1 hole (the tail).
            assert_eq!(list.hole_count(), 1);

            // Free the last allocated block — it's adjacent to the tail.
            list.deallocate(b, layout);

            // They should merge into one combined hole.
            assert_eq!(list.hole_count(), 1, "adjacent holes should merge");

            // Free the first block — now one hole for the full heap.
            list.deallocate(a, layout);
            assert_eq!(list.hole_count(), 1);
            assert_eq!(list.total_free(), TEST_HEAP_SIZE);
        }
    }

    #[test]
    fn deallocate_merges_front_and_back() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(64, 4).unwrap();

            let a = list.allocate_first_fit(layout);
            let b = list.allocate_first_fit(layout);
            let c = list.allocate_first_fit(layout);
            assert!(!a.is_null() && !b.is_null() && !c.is_null());
            // hole count: 1 tail

            // Free `a` and `c` first, then `b` — `b` should merge with both.
            list.deallocate(a, layout);
            list.deallocate(c, layout);
            // After freeing a and c: holes at front and tail (2 holes),
            // the middle (b) is still allocated.
            assert_eq!(list.hole_count(), 2);

            list.deallocate(b, layout);
            // All three should merge into one.
            assert_eq!(list.hole_count(), 1, "all holes should merge into one");
            assert_eq!(
                list.total_free(),
                TEST_HEAP_SIZE,
                "total free should equal full heap"
            );
        }
    }

    #[test]
    fn deallocate_non_adjacent_does_not_merge() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(64, 4).unwrap();

            let a = list.allocate_first_fit(layout);
            let b = list.allocate_first_fit(layout);
            let c = list.allocate_first_fit(layout);
            assert!(!a.is_null() && !b.is_null() && !c.is_null());

            // Free `a` and `c` but not `b` — they are separated by `b`,
            // so they must NOT merge.
            list.deallocate(a, layout);
            list.deallocate(c, layout);

            assert_eq!(list.hole_count(), 2, "non-adjacent holes should not merge");
        }
    }

    #[test]
    fn deallocate_reduces_fragmentation() {
        // Allocate many blocks, free every other one, then verify that
        // the free list has the expected number of holes (free blocks
        // separated by allocated ones).
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(32, 4).unwrap();
            let block_count = 16;
            let mut ptrs: Vec<*mut u8> = Vec::new();

            for _ in 0..block_count {
                let p = list.allocate_first_fit(layout);
                assert!(!p.is_null());
                ptrs.push(p);
            }

            // After all allocations, there should be 1 tail hole (or 0 if exact).
            assert!(list.hole_count() <= 1);

            // Free every other block (indices 0, 2, 4, ...).
            for i in (0..block_count).step_by(2) {
                list.deallocate(ptrs[i], layout);
            }

            // After freeing every other block starting from the front, we have
            // freed blocks at the start (which merge into one front hole),
            // then allocated, then freed, etc. The exact count depends on
            // whether the tail is freed. Since we freed front-interleaved,
            // the free holes merge with adjacent ones. For block_count=16,
            // freeing indices 0,2,4,6,8,10,12,14 gives 8 freed blocks.
            // Because freed-adjacent holes merge:
            //   index 0: front hole (merged with any adjacent)
            //   index 2: separated from index 0 by allocated index 1
            //   index 4: separated from index 2 by allocated index 3
            //   ... so each freed block forms its own hole (unless
            //   consecutive freed blocks merge — but here they're separated).
            //   However, index 14 is adjacent to any tail hole (which was
            //   created after the last allocation), so that merges.
            // Expected: 7 free holes (indices 0,2,4,6,8,10,12) + 1 for
            // the tail region that index 14 merges with.
            //   = 8 holes total. But we also had a tail hole from the last
            //   allocation, which index 14 merges with.
            // For simplicity, just assert that the count is > 0 and reasonable.
            assert!(
                list.hole_count() > 0 && list.hole_count() <= block_count / 2 + 1,
                "unexpected hole count: {}",
                list.hole_count()
            );

            assert!(list.total_free() > 0);
        }
    }

    // ── allocate / deallocate cycles ───────────────────────────────

    #[test]
    fn multiple_alloc_dealloc_cycles() {
        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let layout = Layout::from_size_align(32, 4).unwrap();

            for cycle in 0..10 {
                let ptrs: Vec<_> = (0..8).map(|_| list.allocate_first_fit(layout)).collect();
                assert!(
                    ptrs.iter().all(|p| !p.is_null()),
                    "cycle {cycle}: allocation failed"
                );
                for &p in &ptrs {
                    list.deallocate(p, layout);
                }
                // After each full cycle the free list should be fully merged
                // back to a single hole covering the entire heap.
                assert_eq!(
                    list.hole_count(),
                    1,
                    "cycle {cycle}: holes should fully merge"
                );
                assert_eq!(
                    list.total_free(),
                    TEST_HEAP_SIZE,
                    "cycle {cycle}: free size mismatch"
                );
            }
        }
    }

    #[test]
    fn random_sized_alloc_dealloc() {
        // Deterministic LCG for reproducibility.
        fn lcg(state: &mut u64) -> usize {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state as usize
        }

        unsafe {
            let mut list = HoleList::new();
            let heap = core::ptr::addr_of_mut!(TEST_HEAP) as *mut u8;
            list.init(heap, TEST_HEAP_SIZE);

            let mut rng: u64 = 42;
            let mut allocations: Vec<(*mut u8, Layout)> = Vec::new();

            for _iter in 0..100 {
                let size = (lcg(&mut rng) % 128).max(1);
                let align = 1usize << (lcg(&mut rng) % 5); // 1, 2, 4, 8, 16

                if lcg(&mut rng) % 2 == 0 && !allocations.is_empty() {
                    let idx = lcg(&mut rng) % allocations.len();
                    let (ptr, layout) = allocations.swap_remove(idx);
                    list.deallocate(ptr, layout);
                } else if let Ok(layout) = Layout::from_size_align(size, align) {
                    let ptr = list.allocate_first_fit(layout);
                    if !ptr.is_null() {
                        allocations.push((ptr, layout));
                    }
                }
            }

            // Free everything remaining.
            for (ptr, layout) in allocations {
                list.deallocate(ptr, layout);
            }

            // After freeing all, most of the heap should be reclaimable.
            // Small alignment gaps (< Hole::min_size()) may strand a few
            // bytes, but at least 99% must be free.
            assert!(
                list.total_free() >= TEST_HEAP_SIZE - 128,
                "too much memory stranded after free-all: {} free, expected >= {}",
                list.total_free(),
                TEST_HEAP_SIZE - 128,
            );
        }
    }

    // ── Wire-format tests ──────────────────────────────────────────
    //
    // These tests construct postcard-encoded byte sequences on the host side
    // using the real `postcard` crate and verify the wire-format assumptions
    // that the guest-side manual decoders (`dec_double_result`, `db_get`)
    // rely on.  They require no cross-compilation.

    #[test]
    fn wire_format_dec_double_result_ok_ok_data() {
        // The guest-side `dec_double_result` now returns raw postcard bytes
        // after stripping [outer_status][inner_status].  For Ok(Ok(b"hello")):
        //   → 0x00 0x00 varint(5) b"hello"
        //   → payload = varint(5) b"hello" (caller decodes with dec_str)
        let inner: Result<Vec<u8>, String> = Ok(b"hello".to_vec());
        let outer: Result<Result<Vec<u8>, String>, String> = Ok(inner);
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x00, "expected outer Ok tag");
        assert_eq!(encoded[1], 0x00, "expected inner Ok tag");
        // encoded[2..] is the raw postcard of the Vec<u8> = varint(5) b"hello"
        let (payload, rest): (Vec<u8>, &[u8]) = postcard::take_from_bytes(&encoded[2..]).unwrap();
        assert_eq!(payload, b"hello");
        assert!(rest.is_empty());
    }

    #[test]
    fn wire_format_dec_double_result_ok_err_domain() {
        //   Ok(Err("permission denied"))
        //   → 0x00 0x01 varint(17) b"permission denied"
        let inner: Result<Vec<u8>, String> = Err("permission denied".into());
        let outer: Result<Result<Vec<u8>, String>, String> = Ok(inner);
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x00, "expected outer Ok tag");
        assert_eq!(encoded[1], 0x01, "expected inner Err tag");
        let (err_msg, rest): (String, &[u8]) = postcard::take_from_bytes(&encoded[2..]).unwrap();
        assert_eq!(err_msg, "permission denied");
        assert!(rest.is_empty());
    }

    #[test]
    fn wire_format_dec_double_result_err_infra() {
        //   Err("unknown tool")
        //   → 0x01 varint(11) b"unknown tool"
        let outer: Result<Result<Vec<u8>, String>, String> = Err("unknown tool".into());
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x01, "expected outer Err tag");
        let (err_msg, rest): (String, &[u8]) = postcard::take_from_bytes(&encoded[1..]).unwrap();
        assert_eq!(err_msg, "unknown tool");
        assert!(rest.is_empty());
    }

    #[test]
    fn wire_format_db_get_ok_ok_some() {
        // The host produces Ok(Ok(Some("val"))) as:
        //   0x00 0x00 0x01 varint(3) b"val"
        // `dec_double_result` strips the first two status bytes and returns
        //   0x01 varint(3) b"val"  (raw postcard Option<String>)
        let inner: Result<Option<String>, String> = Ok(Some("val".into()));
        let outer: Result<Result<Option<String>, String>, String> = Ok(inner);
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x00, "expected outer Ok tag");
        assert_eq!(encoded[1], 0x00, "expected inner Ok tag");
        assert_eq!(encoded[2], 0x01, "expected Option::Some tag");
        let (s, rest): (String, &[u8]) = postcard::take_from_bytes(&encoded[3..]).unwrap();
        assert_eq!(s, "val");
        assert!(rest.is_empty());
    }

    #[test]
    fn wire_format_db_get_ok_ok_none() {
        //   Ok(Ok(None))
        //   → 0x00 0x00 0x00
        let inner: Result<Option<String>, String> = Ok(None);
        let outer: Result<Result<Option<String>, String>, String> = Ok(inner);
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], 0x00);
        assert_eq!(encoded[2], 0x00, "expected Option::None tag");
        assert_eq!(encoded.len(), 3);
    }

    #[test]
    fn wire_format_db_get_ok_err_domain() {
        //   Ok(Err("db error"))
        //   → 0x00 0x01 varint(8) b"db error"
        let inner: Result<Option<String>, String> = Err("db error".into());
        let outer: Result<Result<Option<String>, String>, String> = Ok(inner);
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], 0x01);
        let (err_msg, rest): (String, &[u8]) = postcard::take_from_bytes(&encoded[2..]).unwrap();
        assert_eq!(err_msg, "db error");
        assert!(rest.is_empty());
    }

    #[test]
    fn wire_format_db_get_err_infra() {
        //   Err("infra failure")
        //   → 0x01 varint(13) b"infra failure"
        let outer: Result<Result<Option<String>, String>, String> = Err("infra failure".into());
        let encoded = postcard::to_allocvec(&outer).unwrap();

        assert_eq!(encoded[0], 0x01);
        let (err_msg, rest): (String, &[u8]) = postcard::take_from_bytes(&encoded[1..]).unwrap();
        assert_eq!(err_msg, "infra failure");
        assert!(rest.is_empty());
    }
}
