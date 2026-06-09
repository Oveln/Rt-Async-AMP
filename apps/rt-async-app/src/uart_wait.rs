use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use portable_atomic::{AtomicBool, AtomicU16, Ordering};

const BUF_SIZE: usize = 256;
const BUF_MASK: usize = BUF_SIZE - 1;

struct RxInner {
    buf: UnsafeCell<[u8; BUF_SIZE]>,
    head: AtomicU16,
    tail: AtomicU16,
    has_waker: AtomicBool,
    waker: UnsafeCell<MaybeUninit<Waker>>,
}

unsafe impl Sync for RxInner {}
unsafe impl Send for RxInner {}

static RX: RxInner = RxInner {
    buf: UnsafeCell::new([0u8; BUF_SIZE]),
    head: AtomicU16::new(0),
    tail: AtomicU16::new(0),
    has_waker: AtomicBool::new(false),
    waker: UnsafeCell::new(MaybeUninit::uninit()),
};

fn rx_len() -> usize {
    let head = RX.head.load(Ordering::Acquire) as usize;
    let tail = RX.tail.load(Ordering::Acquire) as usize;
    (head.wrapping_sub(tail)) & BUF_MASK
}

pub fn has_byte() -> bool {
    rx_len() > 0
}

pub fn pop_byte() -> Option<u8> {
    let tail = RX.tail.load(Ordering::Acquire) as usize;
    let head = RX.head.load(Ordering::Acquire) as usize;
    if tail == head {
        return None;
    }
    let byte = unsafe { (*RX.buf.get())[tail] };
    RX.tail.store(((tail + 1) & BUF_MASK) as u16, Ordering::Release);
    Some(byte)
}

pub fn push_byte(byte: u8) {
    let head = RX.head.load(Ordering::Acquire) as usize;
    let next = (head + 1) & BUF_MASK;
    let tail = RX.tail.load(Ordering::Acquire) as usize;
    if next == tail {
        return;
    }
    unsafe {
        (*RX.buf.get())[head] = byte;
    }
    RX.head.store(next as u16, Ordering::Release);
}

pub fn notify_from_isr() {
    if RX.has_waker.swap(false, Ordering::AcqRel) {
        unsafe {
            let waker = (*RX.waker.get()).assume_init_read();
            waker.wake();
        }
    }
}

pub struct WaitForByte;

impl Future for WaitForByte {
    type Output = u8;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u8> {
        if let Some(b) = pop_byte() {
            return Poll::Ready(b);
        }

        unsafe { platform::disable_interrupts() };

        unsafe {
            if RX.has_waker.load(Ordering::Relaxed) {
                (*RX.waker.get()).assume_init_drop();
            }
            (*RX.waker.get()).write(cx.waker().clone());
        }
        RX.has_waker.store(true, Ordering::Release);

        if let Some(b) = pop_byte() {
            unsafe {
                RX.has_waker.store(false, Ordering::Relaxed);
                (*RX.waker.get()).assume_init_drop();
            }
            unsafe { platform::enable_interrupts() };
            return Poll::Ready(b);
        }

        unsafe { platform::enable_interrupts() };
        Poll::Pending
    }
}
