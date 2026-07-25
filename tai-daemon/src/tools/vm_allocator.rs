// Linked-list allocator for guest VM programs.
//
// Adapted from the `linked_list_allocator` crate (MIT/Apache-2.0).
// Original: https://github.com/phil-opp/blog_os/tree/main/linked-list-allocator
//
// Uses an intrusive sorted doubly-linked list of free holes stored within
// the free memory itself — no separate allocation for bookkeeping.
//
// Single-threaded: designed for the CKB-VM guest where only one thread runs.
// Lazily initialises the heap on the first allocation or deallocation.

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::cmp::max;
use core::ptr;
use core::ptr::NonNull;



// ── 1 MB heap ─────────────────────────────────────────────────────────────
// With real deallocation (unlike the previous bump allocator) this is
// effectively inexhaustible for any reasonable guest program.
const HEAP_SIZE: usize = 1_048_576;
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

// ── Hole — a free block in the linked list ────────────────────────────────
//
// Each hole is stored inline within the free memory so that no external
// metadata allocation is needed.  Layout within a free block:
//
//   [next: *mut Hole][size: usize][prev: *mut Hole][…free bytes…]
//
// The three pointers/fields occupy 24 bytes on riscv64, which is also the
// minimum hole size.
struct Hole {
    next: Option<NonNull<Hole>>,
    size: usize,
    prev: Option<NonNull<Hole>>,
}

impl Hole {
    fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }

    fn min_size() -> usize {
        Self::header_size()
    }

    /// Minimum alignment required for any Hole struct (8 bytes on 64-bit).
    fn align() -> usize {
        core::mem::align_of::<Self>()
    }

    /// Convenience: round `size` up to maintain Hole alignment so that any
    /// tail hole created after an allocation starts at a valid address.
    fn round_to_align(size: usize) -> usize {
        align_up(size, Self::align())
    }

    /// Pointer to the first byte after this hole's embedded Hole struct.
    fn start(&self) -> *mut u8 {
        unsafe { (self as *const Self as *mut u8).add(Self::header_size()) }
    }

    /// One past the last byte of this hole.
    fn end(&self) -> *mut u8 {
        unsafe { (self as *const Self as *mut u8).add(self.size) }
    }
}

// ── HoleList — sorted intrusive doubly-linked list of free holes ──────────

struct HoleList {
    front: Option<NonNull<Hole>>,
}

impl HoleList {
    const fn new() -> Self {
        HoleList { front: None }
    }

    /// Write a single hole spanning the entire heap region.
    unsafe fn init(&mut self, addr: *mut u8, size: usize) {
        unsafe {
            ptr::write(
                addr as *mut Hole,
                Hole { next: None, size, prev: None },
            );
            self.front = Some(NonNull::new_unchecked(addr as *mut Hole));
        }
    }

    /// First-fit walk: return a pointer to `layout.size()` bytes (at least
    /// `Hole::min_size()`), aligned to `layout.align()`.
    unsafe fn allocate_first_fit(&mut self, layout: Layout) -> *mut u8 {
        unsafe {
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
    }

    /// Return a block of memory back to the free list, merging adjacent holes.
    unsafe fn deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        unsafe {
            // Must use the same rounded size as allocate_first_fit so that
            // the freed block exactly matches the consumed region.
            let size = Hole::round_to_align(max(layout.size(), Hole::min_size()));
            ptr::write(
                ptr as *mut Hole,
                Hole { next: None, size, prev: None },
            );
            self.insert(NonNull::new_unchecked(ptr as *mut Hole));
        }
    }

    // ── list helpers ──────────────────────────────────────────────────

    /// Unlink `hole` from the doubly-linked list.
    unsafe fn remove(&mut self, hole: NonNull<Hole>) {
        unsafe {
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
    }

    /// Insert `hole` into the list at the correct address-sorted position,
    /// then merge with any adjacent holes.
    unsafe fn insert(&mut self, mut hole: NonNull<Hole>) {
        unsafe {
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
    }
}

// ── GlobalAlloc wrapper ───────────────────────────────────────────────────

struct GlobalHeap(UnsafeCell<HoleList>);

// Single-threaded VM guest — safe to be Sync with interior mutability.
unsafe impl Sync for GlobalHeap {}

static mut HEAP_INITIALIZED: bool = false;

/// Lazily initialise the heap on first use so that we don't need to modify
/// the `_start` entry point or wire up an init call.
unsafe fn ensure_heap_initialized() {
    unsafe {
        if !HEAP_INITIALIZED {
            HEAP_INITIALIZED = true;
            let holes = &mut *ALLOC.0.get();
            holes.init(ptr::addr_of_mut!(HEAP) as *mut u8, HEAP_SIZE);
        }
    }
}

unsafe impl GlobalAlloc for GlobalHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            ensure_heap_initialized();
            let holes = &mut *self.0.get();
            holes.allocate_first_fit(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            ensure_heap_initialized();
            let holes = &mut *self.0.get();
            holes.deallocate(ptr, layout);
        }
    }
}

#[global_allocator]
static ALLOC: GlobalHeap = GlobalHeap(UnsafeCell::new(HoleList::new()));

// ── helpers ───────────────────────────────────────────────────────────────

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}
