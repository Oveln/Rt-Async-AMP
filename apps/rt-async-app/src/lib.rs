//! rt-async-amp 双核应用
//!
//! hart 0 (M-mode): rt-async 实时任务
//! hart 1 (S-mode): StarryOS Linux 内核
//! 共享内存 IPC 位于 0x8800_0000

#![no_std]

extern crate alloc;

pub mod intercom;

mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::ptr;

    const HEAP_SIZE: usize = 64 * 1024; // 64KB bump heap

    struct BumpAllocator {
        heap: UnsafeCell<[u8; HEAP_SIZE]>,
        next: UnsafeCell<usize>,
    }

    unsafe impl Sync for BumpAllocator {}

    #[global_allocator]
    static GLOBAL: BumpAllocator = BumpAllocator {
        heap: UnsafeCell::new([0u8; HEAP_SIZE]),
        next: UnsafeCell::new(0),
    };

    unsafe impl GlobalAlloc for BumpAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let next = unsafe { &mut *self.next.get() };
            let heap_start = self.heap.get() as *mut u8;

            let align = layout.align();
            let size = layout.size();

            let current = *next;
            let aligned = (current + align - 1) & !(align - 1);
            let new_next = aligned + size;

            if new_next > HEAP_SIZE {
                return ptr::null_mut();
            }

            *next = new_next;
            unsafe { heap_start.add(aligned) }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // bump allocator: no dealloc
        }
    }
}
