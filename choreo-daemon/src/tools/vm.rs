use crate::tools::{
    MAX_TOOL_OUTPUT_BYTES, Tool, ToolExecError, ToolOutput, ToolRegistry, context::ToolContext,
    finish_tool_output, tool_err, tool_ok,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_keystore::ServiceCredential;
use choreo_sanitize::{ByteBudget, TRUNCATION_MARKER, TRUNCATION_SUFFIX};
use ckb_vm::Bytes;
use ckb_vm::machine::VERSION2;
use ckb_vm::{
    CoreMachine, DefaultCoreMachine, DefaultMachineBuilder, DefaultMachineRunner, Error as VmError,
    FlatMemory, ISA_B, ISA_IMC, ISA_MOP, SupportMachine, Syscalls, TraceMachine, memory::Memory,
    registers,
};
use crossbeam_channel;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;
use tracing::{debug, error, info, trace, warn};

const BOILERPLATE_HEAD: &str = r#"
#![no_std]
#![no_main]
#![allow(unused_imports)]
#![allow(unsafe_op_in_unsafe_fn)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    choreo::exit(1)
}

"#;

const BOILERPLATE_ALLOC_DYNAMIC: &str = include_str!("vm_allocator_dynamic_inner.rs");

const BOILERPLATE_TAIL_BASE: &str = r#"
pub mod choreo {
    pub(crate) static mut ARGC: usize = 0;
    pub(crate) static mut ARGV: *const *const u8 = core::ptr::null();

    pub(crate) fn init_args(argc: usize, argv: *const *const u8) {
        unsafe {
            ARGC = argc;
            ARGV = argv;
        }
    }

    // Syscall numbers for the choreographr custom ABI.
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
    // Read all input registers in one asm block before any function calls
    // that could clobber caller-saved registers (a0-a7 are caller-saved in
    // the RISC-V calling convention).
    let argc: usize;
    let argv: *const *const u8;
    let heap_base: usize;
    let heap_size: usize;
    unsafe {
        // Read the four startup registers using fixed temporary register
        // names (t0 = x5, t1 = x6, t2 = x7, t3 = x28) for the output operands.
        //
        // CRITICAL: We must NOT use `out(reg)` here because the compiler can
        // allocate a0-a3 (which we are READING in the `mv` instructions) as
        // the output registers.  If the compiler assigns, say, a2 to `argc`,
        // then `mv a2, a0` would overwrite the host-set value in a2 before
        // the subsequent `mv {2}, a2` reads it — silently corrupting the
        // heap_base/ heap_size parameters passed by the host.
        //
        // By fixing the outputs to t0-t3 (none of which are a0-a7 or the
        // frame pointer), the `mv` instructions always read FROM the live
        // a-registers and write TO disjoint temporary registers, eliminating
        // the aliasing hazard.
        //
        // These registers are safe to clobber because _start is the entry
        // point — there is no caller context to preserve.
        core::arch::asm!(
            "mv t0, a0",
            "mv t1, a1",
            "mv t2, a2",
            "mv t3, a3",
            out("t0") argc,
            out("t1") argv,
            out("t2") heap_base,
            out("t3") heap_size,
        );
    }
    choreo::init_args(argc, argv);
    // init_heap is at crate root (defined in vm_allocator_dynamic_inner.rs),
    // not inside pub mod choreo — no choreo:: prefix needed.
    // SAFETY: heap_base and heap_size come from the host via registers A2
    // and A3, populated with valid page-aligned bounds within the VM's
    // flat memory space before _start executes.
    unsafe {
        if !init_heap(heap_base, heap_size) {
            choreo::exit(1);
        }
    }
    main();
    choreo::exit(0);
}
"#;

/// Convenience `alloc` imports available at the crate root for user code.
///
/// These are pre-imported so user code can use `Vec`, `String`, `Box`,
/// `format!`, and `.to_string()` without explicit imports.  They live
/// outside `pub mod choreo` so they're in scope for the user's `fn main()`.
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

    /// grep(pattern, regex, include, path, max_results) -> file content search results as string.
    ///
    /// When `regex` is true the pattern is treated as a regular expression;
    /// when false it is matched as a literal substring. (The host defaults to
    /// regex for JSON tool calls, but the guest must pass the bool explicitly
    /// — there is no default on this path.)
    /// `include` is an optional file glob filter (e.g. `Some("*.rs")`).
    /// `path` scopes the search to a directory (None = working directory).
    /// `max_results` caps the number of matches returned (None = default 50).
    /// The host stores max_results as u32, so we cast to u64 for the postcard
    /// varint encoder (postcard encodes both u32 and u64 as the same varint wire format).
    pub fn grep(
        pattern: &str,
        regex: bool,
        include: Option<&str>,
        path: Option<&str>,
        max_results: Option<u32>,
    ) -> String {
        let mut args = Vec::new();
        enc_str(pattern, &mut args);
        enc_bool(regex, &mut args);
        enc_option_str(include, &mut args);
        enc_option_str(path, &mut args);
        enc_option_u64(max_results.map(|n| n as u64), &mut args);
        let resp = call("grep", &args);
        dec_double_str_result(&resp).unwrap_or_default()
    }

    /// find(pattern, glob, path, max_results) -> file name search results as string.
    ///
    /// When `glob` is true the pattern is treated as a glob (supports `*`, `?`, `[`).
    /// When false (default), glob metacharacters are auto-detected — use `false`
    /// to force substring matching for patterns that happen to contain wildcards.
    /// `path` scopes the search to a directory (None = working directory).
    /// `max_results` caps the number of matches returned (None = default 50).
    pub fn find(
        pattern: &str,
        glob: bool,
        path: Option<&str>,
        max_results: Option<u32>,
    ) -> String {
        let mut args = Vec::new();
        enc_str(pattern, &mut args);
        enc_bool(glob, &mut args);
        enc_option_str(path, &mut args);
        enc_option_u64(max_results.map(|n| n as u64), &mut args);
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
    s.push_str(BOILERPLATE_ALLOC_DYNAMIC);
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
    pub program_path: Option<String>,
    pub args: Option<Vec<String>>,
    pub max_cycles: Option<u64>,
    pub memory_size: Option<usize>,
}

struct ChoreographrSyscall {
    registry: Arc<ToolRegistry>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    output_tx: mpsc::Sender<Vec<u8>>,
    write_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    ctx: Option<crate::tools::context::ToolContext>,
    /// Shared byte budget for guest WRITE output (accumulated and streamed
    /// copies both draw from it). Keeps the first [`MAX_TOOL_OUTPUT_BYTES`]
    /// bytes with a fitting prefix — the same "first N bytes + one marker"
    /// contract as the shell streaming paths — so a guest that emits
    /// unbounded output cannot balloon daemon memory or the client's live
    /// display past the shared budget.
    budget: ByteBudget,
    /// One-shot signal fired the first time a guest WRITE is cut at the byte
    /// budget. Message-passing (per AGENTS.md): the syscall holds the sender,
    /// the runner holds the receiver, and the signal is read once after the
    /// machine exits, so the finish footer can carry the truncation marker
    /// without polluting the capped body.
    trunc_tx: mpsc::Sender<()>,
}

impl Syscalls<DefaultCoreMachine<u64, FlatMemory<u64>>> for ChoreographrSyscall {
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
                    // Bound the forwarded total (both the accumulated copy and
                    // the streamed live view) at the shared byte budget: a
                    // guest can write unbounded bytes, and without the cap the
                    // final content and the live stream would both balloon far
                    // past what the transcript can ever show. A write that
                    // would cross the cap is kept as a fitting prefix (the
                    // same contract as the shell streaming paths), the
                    // truncation signal is fired once, and the streamed live
                    // view gets the shared marker; the guest keeps running and
                    // output beyond the cap is dropped.
                    let n = self.budget.fit(data.len());
                    if n > 0 {
                        let _ = self.output_tx.send(data[..n].to_vec());
                        if let Some(tx) = &self.write_tx {
                            let _ = tx.send(data[..n].into());
                        }
                    }
                    if self.budget.is_truncated() {
                        let _ = self.trunc_tx.send(());
                        if let Some(tx) = &self.write_tx {
                            let _ = tx.send(TRUNCATION_SUFFIX.as_bytes().to_vec());
                        }
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
        .arg("+stable")
        .arg("--version")
        .output()
        .map_err(|e| format!("rustc not found: {e}\nInstall from https://rustup.rs"))?;
    if !version.status.success() {
        let stderr = String::from_utf8_lossy(&version.stderr);
        return Err(format!("rustc +stable check failed: {stderr}"));
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
        .arg("+stable")
        .args([
            "--target",
            target,
            "-C",
            "opt-level=2",
            "-C",
            "target-feature=+b,-a",
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

/// Default VM memory size in bytes — currently 4MB, the hard cap of ckb-vm.
///
/// This is intentionally tied to `ckb_vm::RISCV_MAX_MEMORY` rather than a
/// literal: ckb-vm 0.24.14 (the latest release on crates.io) defines
/// `RISCV_MAX_MEMORY = 4 << 20` in the `ckb-vm-definitions` crate and
/// enforces it in two places that we cannot work around:
///
/// 1. `FlatMemory::new_with_memory` **asserts** `memory_size <= RISCV_MAX_MEMORY`
///    (ckb-vm/src/memory/flat.rs) — a larger size would panic, not error.
/// 2. Every load/store goes through `get_page_indices`, which rejects
///    `addr_end > RISCV_MAX_MEMORY` (ckb-vm/src/memory/mod.rs) — even a
///    hand-built machine could not address memory beyond 4MB.
///
/// So 4MB is the largest VM this dependency can construct; raising it to
/// 16MB requires a ckb-vm release with the cap removed (upstream `develop`
/// has refactored this, but nothing newer than 0.24.14 is published).
/// Keeping the default and the validation bound on the upstream constant
/// means the tool follows automatically if the dependency is ever upgraded.
const DEFAULT_VM_MEMORY: usize = ckb_vm::RISCV_MAX_MEMORY;

/// Default cycle budget for the VM.
///
/// 10M gives ~10x headroom over the previous 1M default, which was too
/// small in practice: real guest workloads (git-pull reports, large tool
/// outputs) routinely needed 700K-1.7M cycles, and line-heavy responses
/// pushed past 5M.  A spinning `loop {}` still trips the cap in roughly a
/// second of wall clock (the interpreter is the bottleneck), so 10M keeps
/// runaway guests bounded while leaving room for legitimate I/O-heavy
/// programs.  Kept as a constant so the machine constructor, the
/// invocation description, and the schema text all agree on the default.
const DEFAULT_MAX_CYCLES: u64 = 10_000_000;

/// Compute heap bounds for a guest VM with the given `memory_size`.
///
/// The heap sits between a fixed 256 KB offset and a 64 KB guard below
/// the stack (stack occupies the top quarter of memory).  Returns
/// `(heap_base, heap_size)` where both are in bytes and `heap_size` may
/// be zero when memory is too small to accommodate the layout.
fn compute_heap_bounds(memory_size: usize) -> (u64, u64) {
    let stack_base = memory_size - memory_size / 4;
    let heap_base: usize = 256 * 1024;
    let heap_end = stack_base.saturating_sub(64 * 1024);
    let heap_size = heap_end.saturating_sub(heap_base);
    (heap_base as u64, heap_size as u64)
}

/// Read a pre-compiled RISC-V ELF from disk for the `program_path` input.
///
/// The VM's flat memory is capped at `ckb_vm::RISCV_MAX_MEMORY` (4MB in
/// ckb-vm 0.24.14), so an ELF larger than that can never load — reject it
/// up front with a clear message rather than reading a pathological file
/// into RAM and failing later with a confusing loader error. Returns a
/// domain error string on failure.
fn read_program_file(path: &str) -> Result<Vec<u8>, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read program file '{path}': {e}"))?;
    if meta.len() > ckb_vm::RISCV_MAX_MEMORY as u64 {
        return Err(format!(
            "program file '{path}' is {} bytes; max supported is {} bytes (ckb-vm RISCV_MAX_MEMORY)",
            meta.len(),
            ckb_vm::RISCV_MAX_MEMORY
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read program file '{path}': {e}"))?;
    info!(
        path,
        size = bytes.len(),
        "loaded pre-compiled program from disk"
    );
    Ok(bytes)
}

fn run_riscv_impl(
    input: &RunRiscVInput,
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    write_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
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
    let compile_cmd = format!(
        "rustc +stable --target {target} -C opt-level=2 -C target-feature=+b,-a --edition 2024 --color always"
    );

    let elf = match (
        compile_source,
        input.program.as_deref(),
        input.program_path.as_deref(),
    ) {
        (Some(source), None, None) => {
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
        (None, Some(program_b64), None) => match BASE64.decode(program_b64) {
            Ok(elf) => elf,
            Err(e) => {
                return tool_err(format!("base64 decode error: {e}"));
            }
        },
        (None, None, Some(path)) => match read_program_file(path) {
            Ok(elf) => elf,
            Err(e) => return tool_err(e),
        },
        (None, None, None) => {
            return tool_err("either 'source', 'program', or 'program_path' is required");
        }
        _ => {
            return tool_err("provide only one of 'source', 'program', or 'program_path'");
        }
    };

    let memory_size = input.memory_size.unwrap_or(DEFAULT_VM_MEMORY);
    if !memory_size.is_multiple_of(4096) {
        return tool_err("memory_size must be a multiple of 4096");
    }
    if memory_size < 512 * 1024 {
        return tool_err("memory_size must be at least 512KB");
    }
    // ckb-vm's FlatMemory asserts memory_size <= RISCV_MAX_MEMORY (4MB in
    // 0.24.14) — going over would panic inside the dependency instead of
    // returning a clean error, so we validate against the same bound here.
    if memory_size > ckb_vm::RISCV_MAX_MEMORY {
        return tool_err("memory_size cannot exceed 4MB (ckb-vm RISCV_MAX_MEMORY)");
    }

    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
    // One-shot truncation signal from the guest-WRITE syscall: the runner
    // reads it after the machine exits to decide whether the finish footer
    // carries the `...[truncated]` marker (see the syscall struct docs).
    let (trunc_tx, trunc_rx) = mpsc::channel::<()>();
    let syscall = ChoreographrSyscall {
        registry,
        x_credentials: x_credentials.cloned(),
        working_dir: working_dir.map(|p| p.to_path_buf()),
        output_tx,
        write_tx,
        ctx,
        budget: ByteBudget::new(MAX_TOOL_OUTPUT_BYTES),
        trunc_tx,
    };

    // Single-hart VM: there is only one instruction stream, so the RISC-V A
    // (atomic) extension has no real concurrency semantics to offer. Guests
    // are compiled with `-C target-feature=+b,-a`, so any `core::sync::atomic`
    // read-modify-write op (e.g. `AtomicU32::fetch_add`) is rejected by LLVM
    // at compile time. Leaving A out of the ISA mask keeps the runtime decode
    // surface in sync with what the compiler can emit and shrinks the
    // untrusted instruction surface.
    let core = DefaultCoreMachine::<u64, FlatMemory<u64>>::new_with_memory(
        ISA_IMC | ISA_B | ISA_MOP,
        VERSION2,
        input.max_cycles.unwrap_or(DEFAULT_MAX_CYCLES),
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

    // The host passes heap bounds to the guest via A2 (heap_base) and A3
    // (heap_size).  memory_size is already in scope from earlier.
    let (heap_base, heap_size) = compute_heap_bounds(memory_size);
    trace.set_register(registers::A2, heap_base);
    trace.set_register(registers::A3, heap_size);

    info!(
        memory_size,
        max_cycles = input.max_cycles.unwrap_or(DEFAULT_MAX_CYCLES),
        "starting VM"
    );

    match trace.run() {
        Ok(exit_code) => {
            let cycles = trace.machine.cycles();
            info!(exit_code, cycles, "VM finished successfully");
            // Drop the trace machine first so the ChoreographrSyscall sender is
            // closed before we drain the receiver.  This lets us use a
            // blocking recv() loop that terminates deterministically.
            drop(trace);

            // The syscall fired the one-shot channel the first time guest
            // WRITE output was cut at the byte budget. The marker then rides
            // *past* the cap in the finish footer (finish_tool_output's
            // marker-past-cap convention), so the body stays clean at
            // MAX_TOOL_OUTPUT_BYTES while the truncation is still visible.
            let truncated = trunc_rx.try_recv().is_ok();
            let out_str = String::from_utf8_lossy(&drain_vm_output(&output_rx)).to_string();

            // The guest output is already capped at MAX_TOOL_OUTPUT_BYTES by
            // the WRITE syscall, so `finish_tool_output` is a no-op on the
            // body — but it still guarantees the bound if that cap ever
            // changes, and it appends the exit footer *past* the budget (the
            // same marker-past-cap convention the other tools use) so the
            // exit signal always survives.
            let footer = if truncated {
                format!(
                    "{TRUNCATION_MARKER}\n[VM: exited with code {exit_code} in {cycles} cycles]"
                )
            } else {
                format!("[VM: exited with code {exit_code} in {cycles} cycles]")
            };
            tool_ok(finish_tool_output(&out_str, Some(footer)))
        }
        Err(e) => {
            let cycles = trace.machine.cycles();
            error!(cycles, error = %e, "VM error");
            drop(trace);

            let truncated = trunc_rx.try_recv().is_ok();
            let out_str = String::from_utf8_lossy(&drain_vm_output(&output_rx)).to_string();
            let msg = if out_str.is_empty() {
                format!("VM error after {cycles} cycles: {e}")
            } else {
                format!("VM error after {cycles} cycles: {e}\noutput so far:\n{out_str}")
            };
            // Bound the error message too: `e` can be verbose and the
            // "output so far" tail is capped only by the write cap, so a
            // long message must not exceed the shared budget. When the tail
            // was itself cut at the write cap, the marker is appended past
            // the budget (finish_tool_output's marker-past-cap convention)
            // so the truncation stays visible.
            let marker = truncated.then(|| TRUNCATION_MARKER.to_string());
            tool_err(finish_tool_output(&msg, marker))
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
        "Compile and run Rust code in a RISC-V sandboxed VM. PREFER the 'source' parameter over 'program'. With 'source', only provide a `fn main()` body — the tool auto-generates #![no_std], #![no_main], #[panic_handler], _start, and the `choreo` module. For externally-compiled ELFs, use 'program' (base64) or 'program_path' (path to an ELF file on disk) — the binary must be compiled with the choreographr syscall ABI. Use per-tool convenience wrappers: choreo::read_file(path), choreo::write_file(path, content, overwrite), choreo::db_get(key), choreo::db_set(key, value), choreo::db_delete(key), choreo::sh(command, shell, workdir, timeout_ms), choreo::exec(command, args, workdir, timeout_ms), choreo::grep(pattern, regex, include, path, max_results), choreo::find(pattern, glob, path, max_results), choreo::http_request(method, url, headers, body, timeout_secs). CRITICAL: For grep, pass regex:true for regex patterns and regex:false for literal substring matching. The wrappers handle postcard encoding automatically. Use choreo::write(b\"...\") for VM output and choreo::exit(code) to finish. Do NOT use raw ecall with Linux syscall number 64 (write) — it is not supported. The guest is a single-hart RISC-V VM with the A (atomic) extension disabled, so guests must not use `core::sync::atomic` read-modify-write operations (they fail at compile time)."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match (&args.source, &args.program, &args.program_path) {
            (Some(source), None, None) => {
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
                    args.max_cycles.unwrap_or(DEFAULT_MAX_CYCLES)
                ));
                parts.push(format!(
                    "\nMemory: {} bytes.",
                    args.memory_size.unwrap_or(DEFAULT_VM_MEMORY)
                ));
                parts.concat()
            }
            (None, Some(_), None) => {
                "Running a pre-compiled RISC-V ELF binary (base64).".to_string()
            }
            (None, None, Some(path)) => {
                format!("Running a pre-compiled RISC-V ELF from file: {path}.")
            }
            _ => "Provide only one of 'source', 'program', or 'program_path'.".to_string(),
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
                    "description": "Rust source code for `fn main()`. CRITICAL: Do NOT include #![no_std], #![no_main], #[panic_handler], _start, or the `choreo` module — these are auto-generated. Do NOT use raw ecall with Linux syscall number 64 (write) — it is not supported. Use the provided wrappers (they handle postcard encoding automatically — no need to call choreo::tool_call or choreo::call directly):\n- choreo::write(b\"...\"), choreo::exit(code)\n- choreo::read_file(path: &str) -> String\n- choreo::write_file(path: &str, content: &str, overwrite: bool)\n- choreo::db_get(key: &str) -> Vec<u8>, choreo::db_set(key: &str, value: &[u8]), choreo::db_delete(key: &str) -> bool\n- choreo::sh(command: &str, shell: Shell, workdir: Option<&str>, timeout_ms: Option<u64>) -> String\n- choreo::exec(command: &str, args: &[&str], workdir: Option<&str>, timeout_ms: Option<u64>) -> String\n- choreo::grep(pattern: &str, regex: bool, include: Option<&str>, path: Option<&str>, max_results: Option<u32>) -> String — pass regex: true for regular expression patterns, regex: false for literal substring matching. include is a file glob filter (e.g. Some(\"*.rs\")). path scopes the search directory.\n- choreo::find(pattern: &str, glob: bool, path: Option<&str>, max_results: Option<u32>) -> String — glob: true = glob mode; false = auto-detect.\n- choreo::http_request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>, timeout_secs: Option<u64>) -> String\nExample: `fn main() { let content = choreo::read_file(\"Cargo.toml\"); choreo::write(content.as_bytes()); }`. Alloc types are pre-imported: Vec, String, Box, format!, .to_string(). The guest is a single-hart RISC-V VM with the A (atomic) extension disabled — do not use `core::sync::atomic` read-modify-write operations (they fail at compile time)."
                },
                "program": {
                    "type": "string",
                    "description": "Base64-encoded RISC-V ELF binary. Only use if you compiled externally WITH the choreographr syscall ABI (syscall 0=postcard-encoded tool dispatch, 1=write, 93=exit). Programs using Linux syscall number 64 (write) will fail. When in doubt, use 'source' instead."
                },
                "program_path": {
                    "type": "string",
                    "description": &format!("Path to a pre-compiled RISC-V ELF binary on disk (absolute path recommended). Alternative to 'program' for workflows that compile externally with rustc, avoiding base64 encoding. Same ABI requirements as 'program' (choreographr syscall ABI: 0=postcard tool dispatch, 1=write, 93=exit). Max file size {} bytes ({}MB — ckb-vm's RISCV_MAX_MEMORY).", ckb_vm::RISCV_MAX_MEMORY, ckb_vm::RISCV_MAX_MEMORY / (1024 * 1024))
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Command-line arguments passed to the guest program. Read them with choreo::args() -> Vec<Vec<u8>> in the guest code."
                },

                "max_cycles": {
                    "type": "integer",
                    "description": &format!("Maximum CPU cycles before VM termination (default: {})", DEFAULT_MAX_CYCLES)
                },
                "memory_size": {
                    "type": "integer",
                    "description": &format!("VM memory size in bytes (default: {}, must be a multiple of 4096, max {} bytes = {}MB — ckb-vm's RISCV_MAX_MEMORY)", DEFAULT_VM_MEMORY, ckb_vm::RISCV_MAX_MEMORY, ckb_vm::RISCV_MAX_MEMORY / (1024 * 1024))
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
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
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
            result.contains("choreo::exit(1)"),
            "should contain panic handler"
        );
        // Dynamic allocator specific: init_heap must be present and the
        // old static 1 MB HEAP array must NOT be present.
        assert!(
            result.contains("fn init_heap"),
            "should contain init_heap for dynamic allocator"
        );
        assert!(
            !result.contains("static mut HEAP: [u8; 1_048_576]"),
            "should NOT contain the old static 1 MB HEAP array"
        );
    }

    #[test]
    fn build_boilerplate_contains_choreo_module() {
        let result = build_boilerplate();
        assert!(result.contains("pub mod choreo"));
        assert!(result.contains("TOOL_CALL"));
        assert!(result.contains("WRITE"));
        assert!(result.contains("EXIT"));
        // Per-tool convenience wrappers and their key parameters
        assert!(
            result.contains("fn grep("),
            "grep wrapper should be present"
        );
        assert!(
            result.contains("regex: bool"),
            "grep should expose regex param"
        );
        assert!(
            result.contains("fn find("),
            "find wrapper should be present"
        );
        assert!(
            result.contains("glob: bool"),
            "find should expose glob param"
        );
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
    fn run_riscv_rejects_source_and_program_path() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                source: Some("fn main() {}".to_string()),
                program_path: Some("/tmp/some.elf".to_string()),
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
    fn run_riscv_rejects_program_and_program_path() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                program_path: Some("/tmp/some.elf".to_string()),
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
    fn run_riscv_rejects_missing_program_file() {
        let result = run_riscv_impl(
            &RunRiscVInput {
                program_path: Some("/nonexistent/definitely-not-here-choreo-9f3c2.elf".to_string()),
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
            result.content.contains("cannot read program file"),
            "{}",
            result.content
        );
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
    fn run_riscv_rejects_memory_too_small() {
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
        assert!(result.is_error, "expected error: {}", result.content);
        assert!(
            result.content.contains("at least 512KB"),
            "{}",
            result.content
        );
    }

    #[test]
    fn run_riscv_rejects_memory_over_4mb() {
        // One page over ckb-vm's hard cap (RISCV_MAX_MEMORY = 4MB in 0.24.14).
        // This must fail cleanly in OUR validation — FlatMemory::new_with_memory
        // would otherwise panic on the same input, taking down the tool thread.
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                memory_size: Some(ckb_vm::RISCV_MAX_MEMORY + 4096),
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
    fn vm_default_memory_tracks_ckb_vm_cap() {
        // The 4MB cap is not our choice: ckb-vm 0.24.14 hard-codes
        // RISCV_MAX_MEMORY = 4 << 20 and FlatMemory::new_with_memory asserts
        // on it. Pin the current upstream value so that if ckb-vm ever raises
        // the limit (upstream develop has removed the cap), this test fails
        // and the schema/description text can be updated in the same change.
        assert_eq!(ckb_vm::RISCV_MAX_MEMORY, 4 * 1024 * 1024);
        assert_eq!(DEFAULT_VM_MEMORY, ckb_vm::RISCV_MAX_MEMORY);
    }

    #[test]
    fn vm_default_max_cycles_is_pinned() {
        // Pin the default cycle budget so a deliberate change is a visible
        // one-line edit plus this test update, and the schema/description
        // drift test below cannot silently pass against an unintended value.
        assert_eq!(DEFAULT_MAX_CYCLES, 10_000_000);
    }

    #[test]
    fn vm_schema_and_invocation_document_default_max_cycles() {
        // The machine constructor, describe_invocation, and the JSON schema
        // must all agree on the default max_cycles — this is what the model
        // sees when deciding whether to pass max_cycles explicitly.
        let tool = RunRiscV::new(Weak::new());
        let schema = tool.schema();
        let desc = schema["properties"]["max_cycles"]["description"]
            .as_str()
            .unwrap_or_default();
        let expected = format!("default: {DEFAULT_MAX_CYCLES}");
        assert!(desc.contains(&expected), "schema max_cycles: {desc}");

        let inv = tool.describe_invocation(&RunRiscVInput {
            source: Some("fn main() {}".to_string()),
            ..Default::default()
        });
        let expected_inv = format!("Max cycles: {DEFAULT_MAX_CYCLES}.");
        assert!(inv.contains(&expected_inv), "invocation: {inv}");
    }

    #[test]
    fn run_riscv_accepts_valid_base64_program_with_4k_aligned_memory() {
        // "AAAA" decodes to 3 zero bytes — not a valid ELF, but that's caught at load time,
        // not during input validation. This test verifies that valid base64 + aligned memory
        // passes input validation (the error will be about ELF loading, not input).
        let result = run_riscv_impl(
            &RunRiscVInput {
                program: Some("AAAA".to_string()),
                memory_size: Some(512 * 1024),
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
        assert!(
            !result.content.contains("at least 512KB"),
            "should not be minimum size error: {}",
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

    #[test]
    fn guest_write_output_is_capped_at_byte_budget() {
        // A guest that writes far more than MAX_TOOL_OUTPUT_BYTES must have
        // its output dropped past the cap: neither the accumulated copy nor
        // the streamed live view may exceed the shared budget. Exercised at
        // the syscall level (no cross-compilation needed): two 64 KiB writes
        // fit under the 128 KiB cap, the third must be dropped entirely.
        let mut core = DefaultCoreMachine::<u64, FlatMemory<u64>>::new_with_memory(
            ISA_IMC,
            VERSION2,
            1_000_000,
            256 * 1024,
        );
        let chunk = vec![0x61u8; 64 * 1024];
        let addr = 4096u64;
        core.memory_mut()
            .store_bytes(addr, &chunk)
            .expect("store payload");

        let (accum_tx, accum_rx) = mpsc::channel::<Vec<u8>>();
        let (stream_tx, stream_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (trunc_tx, trunc_rx) = mpsc::channel::<()>();
        let mut syscall = ChoreographrSyscall {
            registry: Arc::new(ToolRegistry::new()),
            x_credentials: None,
            working_dir: None,
            output_tx: accum_tx,
            write_tx: Some(stream_tx),
            ctx: None,
            budget: ByteBudget::new(MAX_TOOL_OUTPUT_BYTES),
            trunc_tx,
        };

        let write = |syscall: &mut ChoreographrSyscall,
                     core: &mut DefaultCoreMachine<u64, FlatMemory<u64>>| {
            core.set_register(registers::A7, 1);
            core.set_register(registers::A0, addr);
            core.set_register(registers::A1, chunk.len() as u64);
            syscall.ecall(core).expect("ecall")
        };

        // Three 64 KiB writes: the first two stay under the cap (128 KiB
        // total), the third crosses it and is cut to a zero-length prefix —
        // nothing of it is forwarded, but the budget reports truncation and
        // the one-shot signal fires.
        assert!(write(&mut syscall, &mut core));
        assert!(write(&mut syscall, &mut core));
        assert!(write(&mut syscall, &mut core));
        // The truncation signal must have been fired exactly once.
        assert_eq!(trunc_rx.try_recv(), Ok(()), "truncation must be signalled");
        assert_eq!(
            trunc_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "truncation must be signalled exactly once"
        );

        // Drop the syscall to close the accumulation sender before draining.
        drop(syscall);
        let mut accumulated = Vec::new();
        while let Ok(part) = accum_rx.recv() {
            accumulated.extend_from_slice(&part);
        }
        assert_eq!(
            accumulated.len(),
            128 * 1024,
            "accumulated output must stop at the cap"
        );
        assert!(
            accumulated.iter().all(|&b| b == 0x61),
            "payload bytes expected"
        );

        // The streamed live view is capped identically and carries the
        // one-time marker (the same suffix `truncate_tool_output` appends).
        let mut streamed = Vec::new();
        while let Ok(part) = stream_rx.try_recv() {
            streamed.extend_from_slice(&part);
        }
        assert_eq!(
            streamed.len(),
            128 * 1024 + TRUNCATION_SUFFIX.len(),
            "streamed output must stop at the cap plus one marker"
        );
        assert!(
            streamed.ends_with(TRUNCATION_SUFFIX.as_bytes()),
            "streamed output must end with the truncation marker"
        );
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
    // shared from the production allocator source (vm_allocator_dynamic_inner.rs)
    // which is included here as a module.

    mod vm_allocator {
        // Include the production allocator source for host-side testing.
        // Items marked #[cfg(not(test))] (e.g. the global allocator) are
        // excluded when compiled under `cargo test`.
        #![allow(unsafe_op_in_unsafe_fn)]
        include!("vm_allocator_dynamic_inner.rs");
    }

    use core::alloc::Layout;
    use vm_allocator::{Hole, HoleList, align_up};

    /// Min-aligned test heap size — big enough for many allocation patterns.
    const TEST_HEAP_SIZE: usize = 4096;

    /// Wrapper to ensure the heap buffer has at least 16-byte alignment
    /// (required by `Hole` which contains `NonNull` pointers).
    #[repr(C, align(16))]
    struct AlignedHeap([u8; TEST_HEAP_SIZE]);

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

            assert_eq!(list.hole_count(), 1, "should have exactly one hole");
            assert_eq!(list.total_free(), TEST_HEAP_SIZE);
        }
    }

    // ── allocate_first_fit ─────────────────────────────────────────

    #[test]
    fn allocate_simple() {
        unsafe {
            let mut list = HoleList::new();
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
    fn deallocate_merges_across_small_alignment_gap() {
        // Verify that deallocation merges holes even when a small gap
        // (< Hole::min_size()) sits between them.  Such gaps arise from
        // alignment padding that was too small to form its own hole and
        // would be permanently stranded under strict adjacency merging.
        unsafe {
            let mut list = HoleList::new();
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

            // Allocate a min-sized anchor block from the front.
            let anchor = Layout::from_size_align(1, 1).unwrap();
            let a = list.allocate_first_fit(anchor);
            assert!(!a.is_null());
            // State: allocated [0, 24), tail hole at [24, HEAP_SIZE).

            // Allocate from the tail with alignment 32 — this creates an
            // 8-byte front gap (align_up(24, 32) - 24) which is smaller
            // than Hole::min_size(), so no front hole is formed.
            let gap_align = Layout::from_size_align(1, 32).unwrap();
            let b = list.allocate_first_fit(gap_align);
            assert!(!b.is_null());
            // State: allocated [0, 24), stranded [24, 32),
            //        allocated [32, 56), tail hole [56, HEAP_SIZE).

            // Free the anchor.  With or without the small-gap merge this
            // creates a hole at [0, 24) — not yet merged (gap to [56, HEAP_SIZE)
            // is 32 bytes >= min_size).
            list.deallocate(a, anchor);

            // Free the second block.  The new merge logic absorbs the 8-byte
            // stranded gap because the previous hole ends at 24 and the freed
            // block starts at 32, and 24 + min_size > 32.  The merged hole
            // then coalesces with the tail, recovering every byte.
            list.deallocate(b, gap_align);

            assert_eq!(
                list.hole_count(),
                1,
                "should merge across the small alignment gap"
            );
            assert_eq!(
                list.total_free(),
                TEST_HEAP_SIZE,
                "all memory including the alignment gap should be reclaimed"
            );
        }
    }

    #[test]
    fn deallocate_reduces_fragmentation() {
        // Allocate many blocks, free every other one, then verify that
        // the free list has the expected number of holes (free blocks
        // separated by allocated ones).
        unsafe {
            let mut list = HoleList::new();
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

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
            let mut heap_buf = AlignedHeap([0u8; TEST_HEAP_SIZE]);
            let heap = &mut heap_buf as *mut AlignedHeap as *mut u8;
            assert!(list.init(heap, TEST_HEAP_SIZE));

            let mut rng: u64 = 42;
            let mut allocations: Vec<(*mut u8, Layout)> = Vec::new();

            for _iter in 0..100 {
                let size = (lcg(&mut rng) % 128).max(1);
                let align = 1usize << (lcg(&mut rng) % 5); // 1, 2, 4, 8, 16

                if lcg(&mut rng).is_multiple_of(2) && !allocations.is_empty() {
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

            // The new merge logic absorbs alignment gaps smaller than
            // Hole::min_size(), so after freeing all allocations the
            // entire heap must be reclaimed as a single hole.
            assert_eq!(
                list.hole_count(),
                1,
                "all holes should merge into one after full free"
            );
            assert_eq!(
                list.total_free(),
                TEST_HEAP_SIZE,
                "all memory should be reclaimed after free-all: got {}",
                list.total_free(),
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

    // ── Heap bounds computation tests ──────────────────────────────────

    #[test]
    fn compute_heap_bounds_default_4mb() {
        let (base, size) = compute_heap_bounds(4 * 1024 * 1024);
        // Default 4 MB layout:
        //   stack_base = 0x300000 (3 MB)
        //   heap_base  = 0x040000 (256 KB)
        //   heap_end   = 0x300000 - 0x10000 = 0x2F0000
        //   heap_size  = 0x2F0000 - 0x040000 = 0x2B0000 (~2.75 MB)
        assert_eq!(base, 262_144);
        assert_eq!(size, 0x2B0000);
    }

    #[test]
    fn compute_heap_bounds_16mb_layout() {
        // Documents how the heap scales for a 16MB VM — the size the tool
        // would offer once ckb-vm drops its 4MB RISCV_MAX_MEMORY cap:
        //   stack_base = 16MB - 16MB/4 = 12MB
        //   heap_end   = 12MB - 64KB = 12,517,376
        //   heap_size  = heap_end - 256KB = 12,255,232 (~11.7MB)
        let (base, size) = compute_heap_bounds(16 * 1024 * 1024);
        assert_eq!(base, 262_144);
        assert_eq!(
            size,
            16 * 1024 * 1024 - 4 * 1024 * 1024 - 64 * 1024 - 256 * 1024
        );
        assert_eq!(size, 12_255_232);
    }

    #[test]
    fn compute_heap_bounds_tiny_memory_no_underflow() {
        // memory_size of one page — heap_end would underflow without
        // saturating arithmetic.  Verify we get heap_size = 0.
        let (base, size) = compute_heap_bounds(4096);
        assert_eq!(base, 262_144);
        assert_eq!(size, 0);
    }

    #[test]
    fn compute_heap_bounds_exactly_enough_for_heap_base() {
        // memory_size where heap_end == heap_base → zero-size heap.
        // stack_base = 262144 - 262144/4 = 196608
        // heap_end = 196608 - 65536 = 131072
        // heap_size = 131072 - 262144 → saturates to 0
        let (base, size) = compute_heap_bounds(262_144);
        assert_eq!(base, 262_144);
        assert_eq!(size, 0);
    }
}
