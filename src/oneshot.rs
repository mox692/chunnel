use crate::loom_wrapper::Arc;
use crate::loom_wrapper::AtomicUsize;
use crate::loom_wrapper::UnsafeCell;
use std::{
    future::Future,
    marker::PhantomData,
    mem::MaybeUninit,
    task::{Poll, Waker},
};

/// Oneshot channel that can send or receive a value only once.
pub fn channel<T>() -> (Tx<T>, Rx<T>) {
    let inner = Arc::new(Inner {
        state: AtomicUsize::new(0),
        bucket: UnsafeCell::new(MaybeUninit::uninit()),
        waker: UnsafeCell::new(None),
    });
    let tx = Tx {
        inner: inner.clone(),
        _p: PhantomData,
    };
    let rx = Rx {
        inner: inner.clone(),
        _p: PhantomData,
    };
    (tx, rx)
}

// Inner states
const SENDER_SET: usize = 0b001;
const RECEIVER_WAIT: usize = 0b010;
const COMPLETE: usize = 0b100;

struct Inner<T> {
    state: AtomicUsize,
    bucket: UnsafeCell<MaybeUninit<T>>,
    waker: UnsafeCell<Option<Waker>>,
}

/// Sender handle of the channel. This implements `Send` but not `Sync`.
pub struct Tx<T> {
    inner: Arc<Inner<T>>,

    // To make `Tx` !Sync
    _p: PhantomData<*const ()>,
}

unsafe impl<T: Send> Send for Tx<T> {}

/// Sender handle of the channel. This implements `Send` but not `Sync`.
pub struct Rx<T> {
    inner: Arc<Inner<T>>,

    // To make `Tx` !Sync
    _p: PhantomData<*const ()>,
}

unsafe impl<T: Send> Send for Rx<T> {}

impl<T> Tx<T> {
    pub fn send(&self, val: T) {
        let state = self.inner.state.load(std::sync::atomic::Ordering::Acquire);

        if state & SENDER_SET != 0 {
            // We've already sent a value to this channel, so do nothing.
            return;
        }

        // Set a value.
        //
        // Since we don't allow the `Tx` to have multiple clones,
        // it's ok to just set the value without any syncronization.
        // let ptr = self.inner.bucket.get() as *mut T;
        self.inner.bucket.with_mut(|ptr| {
            // SAFETY: todo
            unsafe {
                (ptr as *mut T).write(val);
            }
        });

        // Change the flag.

        let mut cur = state;
        let mut next = state | SENDER_SET;
        while let Err(now) = self.inner.state.compare_exchange_weak(
            cur,
            next,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            // Receiver has just updated the inner value, so try again.

            assert!(now & SENDER_SET == 0);

            cur = now;
            next = cur | SENDER_SET;
        }

        if next & RECEIVER_WAIT == RECEIVER_WAIT {
            // wakeup the receiver
            self.inner.waker.with_mut(|w| {
                // SAFETY:
                unsafe { w.as_ref() }
                    .expect("Waker field should not be null")
                    .as_ref()
                    .expect("Waker should exist at this point")
                    .wake_by_ref();
            });
        }
    }
}

impl<T> Rx<T> {
    /// Sync variant
    pub fn try_recv(&self) -> Option<T> {
        let state = self.inner.state.load(std::sync::atomic::Ordering::Acquire);

        // Check if the sender is set and the operation is not complete
        if state & SENDER_SET != SENDER_SET || state & COMPLETE == COMPLETE {
            return None;
        }

        // Try to update the state
        if self
            .inner
            .state
            .compare_exchange_weak(
                state,
                COMPLETE,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return None;
        }

        // Read the value
        let value = self
            .inner
            .bucket
            .with(|ptr| unsafe { (ptr as *const T).read() });

        Some(value)
    }

    /// Async variant
    pub async fn recv(&mut self) -> Option<T> {
        // TODO: stack pinning.
        // After this recv call, users should not be able to
        // move this Rx, so it's safe to pin itself on the stack.
        let this = Box::pin(self);
        this.await
    }
}

impl<T> Future for Rx<T> {
    type Output = Option<T>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let state = self.inner.state.load(std::sync::atomic::Ordering::Acquire);
        let sender_set = state & SENDER_SET == SENDER_SET;
        let complete = state & COMPLETE == COMPLETE;

        if complete {
            return Poll::Ready(None);
        }

        if sender_set {
            // Since we have a exclusive ownership of the receiver,
            // we don't have to perform CAS loop.
            let value = self
                .inner
                .bucket
                .with(|ptr| unsafe { (ptr as *const T).read() });

            // Set COMPLETE flag
            self.inner
                .state
                .store(COMPLETE, std::sync::atomic::Ordering::Release);

            return Poll::Ready(Some(value));
        }

        // Set the waker
        // TODO: update the waker only if task has been changed.
        let waker = cx.waker().clone();
        let this = self.get_mut();
        unsafe {
            this.inner
                .waker
                .with_mut(|old_waker| *old_waker = Some(waker))
        }

        // Try to set the RECEIVER_WAIT flag
        let cur = state;
        let next = state | RECEIVER_WAIT;
        match this.inner.state.compare_exchange_weak(
            cur,
            next,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => Poll::Pending,
            Err(now) => {
                // Sender has just updated the inner value!
                // SENDER_SET should be set
                assert!(now & SENDER_SET == 1);

                // Get the inner value
                let value = this
                    .inner
                    .bucket
                    .with(|ptr| unsafe { (ptr as *const T).read() });

                // Set COMPLETE flag
                this.inner
                    .state
                    .store(COMPLETE, std::sync::atomic::Ordering::Release);

                Poll::Ready(Some(value))
            }
        }
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use super::*;
    use loom::{future::block_on, thread};

    #[test]
    fn recv() {
        loom::model(|| {
            let (tx, mut rx) = channel();

            let jh = thread::spawn(move || {
                block_on(async {
                    let res = rx.recv().await.unwrap();
                    assert_eq!(res, 42);
                });
            });

            block_on(async {
                tx.send(42);
            });

            jh.join().unwrap();
        });
    }
}
