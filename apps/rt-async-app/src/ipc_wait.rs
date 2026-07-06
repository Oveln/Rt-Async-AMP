use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use portable_atomic::{AtomicBool, Ordering};

use crate::intercom;

struct Inner {
    has_waker: AtomicBool,
    waker: UnsafeCell<MaybeUninit<Waker>>,
}

unsafe impl Sync for Inner {}
unsafe impl Send for Inner {}

static INNER: Inner = Inner {
    has_waker: AtomicBool::new(false),
    waker: UnsafeCell::new(MaybeUninit::uninit()),
};

pub fn notify_from_isr() {
    if !intercom::has_pending() {
        return;
    }
    if INNER.has_waker.swap(false, Ordering::AcqRel) {
        unsafe {
            let waker = (*INNER.waker.get()).assume_init_read();
            waker.wake();
        }
        // Signal to the MachineSoft handler that a task is ready to run.
        // Without this, clear_pend() returns false for external IPIs and
        // the scheduler never gets to execute the woken task.
        platform::PEND_MARKER.store(true, Ordering::Release);
    }
}

pub struct WaitForMessage;

impl Future for WaitForMessage {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if intercom::has_pending() {
            return Poll::Ready(());
        }

        unsafe { platform::disable_interrupts() };

        unsafe {
            if INNER.has_waker.load(Ordering::Relaxed) {
                (*INNER.waker.get()).assume_init_drop();
            }
            (*INNER.waker.get()).write(cx.waker().clone());
        }
        INNER.has_waker.store(true, Ordering::Release);

        if intercom::has_pending() {
            unsafe {
                INNER.has_waker.store(false, Ordering::Relaxed);
                (*INNER.waker.get()).assume_init_drop();
            }
            unsafe { platform::enable_interrupts() };
            return Poll::Ready(());
        }

        unsafe { platform::enable_interrupts() };
        Poll::Pending
    }
}
