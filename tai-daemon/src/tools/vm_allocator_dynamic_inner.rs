// Linked-list allocator for guest VM programs — dynamic heap bounds variant.
//
// Adapted from the `linked_list_allocator` crate (MIT/Apache-2.0).
// Original: https://github.com/phil-opp/blog_os/tree/main/linked-list-allocator
//
// Uses an intrusive sorted doubly-linked list of free holes stored within
// the free memory itself — no separate allocation for bookkeeping.
//
// Single-threaded: designed for the CKB-VM guest where only one thread runs.
// Unlike vm_allocator_inner.rs, this variant has **no static 1 MB heap array**.
// Instead, the heap is initialised at runtime by `init_heap()` with bounds
// passed by the host via registers A2 (heap_base) and A3 (heap_size).

// The unsafe operations inside unsafe fn are intentional — wrapping each
// in a redundant unsafe block adds noise without benefit.
// Suppressed via #[allow] on each unsafe fn below.

// Items below marked `#[cfg(not(test))]` are guest-only (heap + global allocator).
// When this file is compiled for host-side testing (via include! in vm.rs's
// #[cfg(test)] module), those items are excluded to avoid conflicts with the
// host's global allocator and to suppress unused-import warnings.

#[allow(dead_code, unused_imports)]
extern crate alloc;

#[allow(dead_code, unused_imports)]
use core::alloc::{GlobalAlloc, Layout};
#[allow(dead_code, unused_imports)]
use core::cell::UnsafeCell;
use core::cmp::max;
use core::ptr;
use core::ptr::NonNull;

// ── No static HEAP array — heap bounds provided at runtime via init_heap() ──

// ── Hole — a free block in the linked list ────────────────────────────────
//
// Each hole is stored inline within the free memory so that no external
// metadata allocation is needed.  Layout within a free block:
//
//   [next: *mut Hole][size: usize][prev: *mut Hole][…free bytes…]
//
// The three pointers/fields occupy 24 bytes on riscv64, which is also the
// minimum hole size.
pub(crate) struct Hole {
    next: Option<NonNull<Hole>>,
    size: usize,
    prev: Option<NonNull<Hole>>,
}

impl Hole {
    pub fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }

    pub fn min_size() -> usize {
        Self::header_size()
    }

    /// Minimum alignment required for any Hole struct (8 bytes on 64-bit).
    pub fn align() -> usize {
        core::mem::align_of::<Self>()
    }

    /// Convenience: round `size` up to maintain Hole alignment so that any
    /// tail hole created after an allocation starts at a valid address.
    pub fn round_to_align(size: usize) -> usize {
        align_up(size, Self::align())
    }

    /// Pointer to the first byte after this hole's embedded Hole struct.
    pub fn start(&self) -> *mut u8 {
        unsafe { (self as *const Self as *mut u8).add(Self::header_size()) }
    }

    /// One past the last byte of this hole.
    pub fn end(&self) -> *mut u8 {
        unsafe { (self as *const Self as *mut u8).add(self.size) }
    }
}

// ── HoleList — sorted intrusive doubly-linked list of free holes ──────────

pub(crate) struct HoleList {
    pub front: Option<NonNull<Hole>>,
}

impl HoleList {
    pub const fn new() -> Self {
        HoleList { front: None }
    }

    /// Write a single hole spanning the entire heap region.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn init(&mut self, addr: *mut u8, size: usize) {
        ptr::write(
            addr as *mut Hole,
            Hole { next: None, size, prev: None },
        );
        self.front = Some(NonNull::new_unchecked(addr as *mut Hole));
    }

    /// First-fit walk: return a pointer to `layout.size()` bytes (at least
    /// `Hole::min_size()`), aligned to `layout.align()`.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn allocate_first_fit(&mut self, layout: Layout) -> *mut u8 {
        // The allocation must be at least min_size and must maintain
        // Hole alignment so that any tail hole starts at a valid address.
        let size = Hole::round_to_align(max(layout.size(), Hole::min_size()));
        // We must align to at least Hole::align() so that the returned
        // pointer and any tail hole address are always valid for storing
        // a Hole struct.
        let effective_align = max(layout.align(), Hole::align());

        let mut current = self.front;
        while let Some(hole_ptr) = current {
            let hole = &*hole_ptr.as_ptr();

            let hole_addr = hole_ptr.as_ptr() as usize;
            let hole_end_addr = hole_addr.wrapping_add(hole.size);

            // The allocation starts from the hole's own address (not after
            // the header) so the header bytes are reused as part of the
            // allocation payload, avoiding the leak that would occur if
            // we started from hole.start().
            let aligned = align_up(hole_addr, effective_align) as *mut u8;
            let aligned_addr = aligned as usize;
            let alloc_end = aligned_addr.wrapping_add(size);

            // Would the aligned allocation fit?
            if aligned_addr < hole_addr
                || aligned_addr >= hole_end_addr
                || alloc_end > hole_end_addr
            {
                current = hole.next;
                continue;
            }

            // We can allocate here. Remove this hole from the list.
            self.remove(hole_ptr);

            // If there is reclaimable space before the aligned pointer,
            // turn it into a new hole.
            let front = aligned_addr.wrapping_sub(hole_addr);
            if front >= Hole::min_size() {
                let front_hole = hole_addr as *mut Hole;
                ptr::write(
                    front_hole,
                    Hole { next: None, size: front, prev: None },
                );
                self.insert(NonNull::new_unchecked(front_hole));
            }

            // If there is reclaimable space after the allocation, turn it
            // into a new hole.
            let tail = hole_end_addr.wrapping_sub(alloc_end);
            if tail >= Hole::min_size() {
                let tail_hole = aligned.add(size) as *mut Hole;
                ptr::write(
                    tail_hole,
                    Hole { next: None, size: tail, prev: None },
                );
                self.insert(NonNull::new_unchecked(tail_hole));
            }

            return aligned;
        }

        ptr::null_mut()
    }

    /// Return a block of memory back to the free list, merging adjacent holes.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        // Must use the same rounded size as allocate_first_fit so that
        // the freed block exactly matches the consumed region.
        let size = Hole::round_to_align(max(layout.size(), Hole::min_size()));
        ptr::write(
            ptr as *mut Hole,
            Hole { next: None, size, prev: None },
        );
        self.insert(NonNull::new_unchecked(ptr as *mut Hole));
    }

    // ── list helpers ──────────────────────────────────────────────────

    /// Unlink `hole` from the doubly-linked list.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn remove(&mut self, hole: NonNull<Hole>) {
        let prev = (*hole.as_ptr()).prev;
        let next = (*hole.as_ptr()).next;

        if let Some(p) = prev {
            (*p.as_ptr()).next = next;
        } else {
            self.front = next;
        }
        if let Some(n) = next {
            (*n.as_ptr()).prev = prev;
        }
    }

    /// Insert `hole` into the list at the correct address-sorted position,
    /// then merge with any adjacent holes.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn insert(&mut self, mut hole: NonNull<Hole>) {
        let hole_addr = hole.as_ptr() as usize;

        // Walk to find the insertion point (address-sorted).
        let mut current = self.front;
        let mut prev: Option<NonNull<Hole>> = None;
        while let Some(curr) = current {
            if curr.as_ptr() as usize > hole_addr {
                break;
            }
            prev = current;
            current = (*curr.as_ptr()).next;
        }

        // Link hole between prev and current.
        (*hole.as_ptr()).prev = prev;
        (*hole.as_ptr()).next = current;
        if let Some(p) = prev {
            (*p.as_ptr()).next = Some(hole);
        } else {
            self.front = Some(hole);
        }
        if let Some(c) = current {
            (*c.as_ptr()).prev = Some(hole);
        }

        // Merge with previous hole if adjacent.
        if let Some(p) = prev {
            let p_off = p.as_ptr() as usize;
            let p_sz = (*p.as_ptr()).size;
            let h_off = hole.as_ptr() as usize;
            let h_sz = (*hole.as_ptr()).size;
            if p_off.wrapping_add(p_sz) >= h_off {
                let new_sz = max(p_off.wrapping_add(p_sz), h_off.wrapping_add(h_sz)) - p_off;
                (*p.as_ptr()).size = new_sz;
                (*p.as_ptr()).next = (*hole.as_ptr()).next;
                if let Some(n) = (*hole.as_ptr()).next {
                    (*n.as_ptr()).prev = Some(p);
                }
                hole = p;
            }
        }

        // Merge with next hole if adjacent.
        if let Some(n) = (*hole.as_ptr()).next {
            let h_off = hole.as_ptr() as usize;
            let h_sz = (*hole.as_ptr()).size;
            let n_off = n.as_ptr() as usize;
            let n_sz = (*n.as_ptr()).size;
            if h_off.wrapping_add(h_sz) >= n_off {
                let new_sz = max(h_off.wrapping_add(h_sz), n_off.wrapping_add(n_sz)) - h_off;
                (*hole.as_ptr()).size = new_sz;
                (*hole.as_ptr()).next = (*n.as_ptr()).next;
                if let Some(nn) = (*n.as_ptr()).next {
                    (*nn.as_ptr()).prev = Some(hole);
                }
            }
        }
    }

    /// Count holes in the list (for assertions in host tests).
    pub fn hole_count(&self) -> usize {
        let mut count = 0;
        let mut cur = self.front;
        while let Some(c) = cur {
            count += 1;
            cur = unsafe { (*c.as_ptr()).next };
        }
        count
    }

    /// Total free bytes (sum of hole sizes, for assertions in host tests).
    pub fn total_free(&self) -> usize {
        let mut total = 0;
        let mut cur = self.front;
        while let Some(c) = cur {
            total += unsafe { (*c.as_ptr()).size };
            cur = unsafe { (*c.as_ptr()).next };
        }
        total
    }
}

// ── GlobalAlloc wrapper (guest only) ──────────────────────────────────────

#[cfg(not(test))]
struct GlobalHeap(UnsafeCell<HoleList>);

#[cfg(not(test))]
// Safety: the VM guest runs a single hart (thread), so there is no
// concurrent access to the HoleList.  The UnsafeCell interior mutability
// is exercised only by the single executing thread, making Sync sound.
unsafe impl Sync for GlobalHeap {}

#[cfg(not(test))]
/// Tracks whether `init_heap` has been called.
static mut HEAP_INITIALIZED: bool = false;

#[cfg(not(test))]
/// Initialise the heap allocator with host-provided bounds.
///
/// Called from `_start` (in BOILERPLATE_TAIL_CLOSE) after reading
/// heap_base from register A2 and heap_size from register A3.
/// This function is at crate root (not inside `pub mod tai`).
///
/// # Safety
///
/// - `base` must point to a valid, writable memory region of at least
///   `size` bytes within the VM's flat memory space.
/// - `base` must be aligned to `core::mem::align_of::<Hole>()` (8 bytes
///   on 64-bit RISC-V).
/// - This function must be called exactly once before any call to
///   the global allocator (`alloc`/`dealloc`).
/// - Calling this function more than once without first exhausting all
///   allocations is undefined behaviour (the existing free list is
///   overwritten).
pub unsafe fn init_heap(base: usize, size: usize) {
    let holes = &mut *ALLOC.0.get();
    holes.init(base as *mut u8, size);
    HEAP_INITIALIZED = true;
}

#[cfg(not(test))]
/// Guard: bails out early when `init_heap` has not been called.
/// Returns `true` if the heap is ready, `false` if not.
unsafe fn ensure_heap_initialized() -> bool {
    HEAP_INITIALIZED
}

#[cfg(not(test))]
unsafe impl GlobalAlloc for GlobalHeap {
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !ensure_heap_initialized() {
            return ptr::null_mut();
        }
        let holes = &mut *self.0.get();
        holes.allocate_first_fit(layout)
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !ensure_heap_initialized() {
            return;
        }
        let holes = &mut *self.0.get();
        holes.deallocate(ptr, layout);
    }
}

#[cfg(not(test))]
#[global_allocator]
static ALLOC: GlobalHeap = GlobalHeap(UnsafeCell::new(HoleList::new()));

// ── helpers ───────────────────────────────────────────────────────────────

pub fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}
