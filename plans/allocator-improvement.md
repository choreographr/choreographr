# Allocator Improvement Plan

## Problem

The guest VM allocator (`vm_allocator.rs`) uses a **fixed 1 MB static `[u8; 1_048_576]` array** in `.bss`. This:

1. **Wastes memory**: The 1 MB is reserved unconditionally, even for trivial programs that allocate only a few bytes.
2. **Fixed ceiling**: Programs that need more than 1 MB of heap are out of luck, even though ~2.8 MB of VM memory sits completely unused between the end of the ELF and the stack region.
3. **Inflates `.bss`**: The 1 MB contributes to the guest's load-time data segment.

## Non-Goal

- **Do NOT modify `vm_allocator.rs`** — it is vendored code (adapted from `linked_list_allocator`). The `Hole`, `HoleList`, `GlobalHeap`, `align_up` types and their internal logic stay exactly as they are.

## Approach

Replace `BOILERPLATE_ALLOC` (currently `include_str!("vm_allocator.rs")`) with a **new dynamic allocator module** that is structurally identical to the vendored one but receives its heap bounds from the host at startup via **registers A2 and A3**.

### 1. Host-Side: Pass Heap Bounds in Registers (`vm.rs`)

In `run_riscv_impl`, after `trace.load_program(...)` succeeds and before `trace.run()`, the host sets:

```
A0 = argc        (already done)
A1 = argv        (already done)
A2 = heap_base   (start of available heap memory)
A3 = heap_size   (size of heap region in bytes)
```

**Computing `heap_base` and `heap_size`:**

The host knows:
- `memory_size` (the total VM flat memory, default 4 MB, configurable)
- `stack_base = memory_size - memory_size / 4` (3 MB for default 4 MB)

The heap lives between the end of the loaded ELF and the stack, with a 64 KB guard:

Let's use the ELF itself to determine where it ends:

- Parse the ELF program headers from the compiled binary (in `compile()` or `run_riscv_impl`)
- Find `max_end = max(p_vaddr + p_memsz)` across all `PT_LOAD` segments
- `heap_base = page_align_up(max_end)` (round up to 4 KB)
- `heap_size = (stack_base - 64 * 1024) - heap_base` (leave 64 KB guard below stack)

**Implementation** (in `run_riscv_impl`, after `trace.load_program`):

```rust
let heap_base = input.memory_size.unwrap_or(4 * 1024 * 1024) / 4;
let heap_end = input.memory_size.unwrap_or(4 * 1024 * 1024) - 64 * 1024;
trace.set_register(registers::A2, heap_base as u64);
trace.set_register(registers::A3, (heap_end - heap_base) as u64);
```

As a simplification for the first iteration, we can use a **fixed generous offset** (e.g., 256 KB from the start of memory) as `heap_base` rather than parsing ELF headers. For the current `riscv64imac-unknown-none-elf` target with default linker settings, the entire loaded ELF (code + data + bss) fits comfortably in 256 KB.

### 2. Guest-Side: `_start` Receives Bounds (`vm.rs`)

Modify the guest's `_start` function (in `BOILERPLATE_TAIL_CLOSE`) to read the heap bounds from A2/A3 and relay them to the allocator:

```rust
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
    // The host passes heap_base in A2 and heap_size in A3.
    let heap_base: usize;
    let heap_size: usize;
    unsafe {
        core::arch::asm!(
            "mv {base}, a2",
            "mv {size}, a3",
            base = out(reg) heap_base,
            size = out(reg) heap_size,
        );
    }
    unsafe { tai::init_heap(heap_base, heap_size); }
    main();
    tai::exit(0);
}
```

### 3. Guest-Side: `init_heap()` Function (`vm.rs`)

Add a new `tai::init_heap()` function that replaces the lazy-init path in `ensure_heap_initialized`. Since we cannot modify `vm_allocator.rs`, we write a **new dynamic allocator module** in a new file (e.g., `vm_allocator_dynamic.rs`) that replaces `include_str!("vm_allocator.rs")`:

The new module is structurally identical to the vendored one, except:
- No `static mut HEAP: [u8; HEAP_SIZE]` array
- No hardcoded `HEAP_SIZE` constant
- `ensure_heap_initialized` accepts `(base, size)` parameters instead of using the static array

```rust
// New: called from _start before main()
pub fn init_heap(base: usize, size: usize) {
    unsafe {
        let holes = &mut *ALLOC.0.get();
        holes.init(base as *mut u8, size);
        HEAP_INITIALIZED = true;
    }
}
```

The rest of the module (Hole, HoleList, GlobalAlloc impl, align_up) remains exactly the same as `vm_allocator.rs`.

### 4. Building the Boilerplate (`vm.rs`)

Update `build_boilerplate()`:

```rust
fn build_boilerplate() -> String {
    let mut s = String::from(BOILERPLATE_HEAD);
    s.push_str(BOILERPLATE_ALLOC_DYNAMIC);  // <-- new file instead of vm_allocator.rs
    s.push_str(BOILERPLATE_TAIL_BASE);
    s.push_str(BOILERPLATE_TAIL_ALLOC);
    s.push_str(BOILERPLATE_TAIL_ENCODING);
    s.push_str(BOILERPLATE_TAIL_CLOSE);
    s.push_str(BOILERPLATE_CONVENIENCE_IMPORTS);
    s
}
```

### 5. Memory Layout (Default 4 MB)

```
0x000000 ┌─────────────────────┐
         │  ELF segments       │  (~8-200 KB: code, rodata, data, .bss)
         │  (no 1 MB static    │
         │   heap in .bss)     │
0x040000 ├─────────────────────┤  ← heap_base (256 KB fixed offset)
         │                     │
         │    ~2.7 MB heap     │
         │                     │
0x2F0000 ├─────────────────────┤  ← stack_base - 64 KB guard
         │   64 KB guard       │
0x300000 ├─────────────────────┤  ← stack_base (= memory_size - memory_size/4)
         │   1 MB stack        │
0x400000 └─────────────────────┘  ← end of memory
```

### 6. Test Impact

| Test | Action |
|------|--------|
| `build_boilerplate_includes_allocator` | Update assertion. Still checks `struct HoleList`, `fn args()`, `fn _start()`, `tai::exit(1)` |
| `build_boilerplate_contains_tai_module` | Unchanged |
| All `HoleList` unit tests (host-side replicas at lines 1690–2563) | Unchanged — these re-implement `HoleList` for native testing and don't depend on the static HEAP |
| Integration tests (`tests/vm_integration.rs`) | Unchanged — functional behaviour is identical |

### 7. Steps

1. Create `tai-daemon/src/tools/vm_allocator_dynamic.rs` — copy of `vm_allocator.rs` with static HEAP removed and `init_heap(base, size)` added.
2. Add `const BOILERPLATE_ALLOC_DYNAMIC: &str = include_str!("vm_allocator_dynamic.rs");` in `vm.rs`.
3. Update `build_boilerplate()` to use `BOILERPLATE_ALLOC_DYNAMIC`.
4. Modify `_start` in `BOILERPLATE_TAIL_CLOSE` to read A2/A3 and call `tai::init_heap()`.
5. Add `pub fn init_heap(base: usize, size: usize)` in `BOILERPLATE_TAIL_ALLOC` (within `pub mod tai`).
6. In `run_riscv_impl`, set A2 and A3 after `load_program`.
7. Update the `build_boilerplate_includes_allocator` test.
8. `cargo check`, `cargo test`, `cargo clippy`, `cargo fmt --check`.
9. Run integration tests: `cargo test -- --ignored`.

### 8. Future Considerations

- **ELF header parsing**: For maximum heap availability, the host could parse the compiled ELF's `PT_LOAD` headers to compute the exact end of loaded segments instead of using a fixed offset.
- **Configurable heap fraction**: The heap/stack split (`memory_size / 4` for stack) could be exposed as a `RunRiscVInput` parameter.
- **Original vendored file**: `vm_allocator.rs` remains unmodified on disk, preserving provenance tracking for the upstream `linked_list_allocator`.
