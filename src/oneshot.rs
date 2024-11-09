use std::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    mem::MaybeUninit,
    rc::Rc,
    sync::{atomic::AtomicU8, Arc},
    task::{Poll, Waker},
};

/// Oneshot channel that can send or receive a value only once.
pub fn channel<T>() -> (Tx<T>, Rx<T>) {
    let inner = Arc::new(Inner {
        state: AtomicU8::new(0),
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

struct Inner<T> {
    state: AtomicU8,
    bucket: UnsafeCell<MaybeUninit<T>>,
    waker: UnsafeCell<Option<Waker>>,
}

/// Sender handle of the channel. This implements `Send` but not `Sync`.
pub struct Tx<T> {
    inner: Arc<Inner<T>>,

    // To make `Tx` !Sync
    _p: PhantomData<Rc<()>>,
}

unsafe impl<T: Send> Send for Tx<T> {}

/// Sender handle of the channel. This implements `Send` but not `Sync`.
pub struct Rx<T> {
    inner: Arc<Inner<T>>,

    // To make `Tx` !Sync
    _p: PhantomData<Rc<()>>,
}

unsafe impl<T: Send> Send for Rx<T> {}

impl<T> Tx<T> {
    pub fn send(&self, val: T) {
        if self.inner.state.load(std::sync::atomic::Ordering::Acquire) == 0 {
            // Set a value.
            //
            // Since we don't allow the `Tx` to have multiple clones,
            // it's ok to just set the value without any syncronization.
            let ptr = self.inner.bucket.get() as *mut T;
            unsafe {
                ptr.write(val);
            }

            // Change the flag
            self.inner
                .state
                .store(1, std::sync::atomic::Ordering::Release);
        } else {
            // We've already sent a value to this channel, so do nothing.
        }
    }
}

impl<T> Rx<T> {
    /// Sync variant
    pub fn try_recv(&self) -> Option<T> {
        if self.inner.state.load(std::sync::atomic::Ordering::Acquire) == 1 {
            // read the value
            let ptr = self.inner.bucket.get() as *const T;
            let value = unsafe { ptr.read() };

            // set finalize flag
            self.inner
                .state
                .store(2, std::sync::atomic::Ordering::Release);

            Some(value)
        } else {
            // complete or not set yet.
            None
        }
    }

    /// Async variant
    pub async fn recv(&mut self) -> Option<T> {
        let this = Box::pin(self);
        this.await
    }
}

impl<T> Future for Rx<T> {
    type Output = Option<T>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let state = self.inner.state.load(std::sync::atomic::Ordering::Acquire);

        if state == 1 {
            // Read the value
            let ptr = self.inner.bucket.get() as *const T;
            let value = unsafe { ptr.read() };

            // Set finalize flag
            self.inner
                .state
                .store(2, std::sync::atomic::Ordering::Release);

            Poll::Ready(Some(value))
        } else if state == 0 {
            // Set the waker
            // TODO: update the waker only if task has been changed.
            let waker = cx.waker().clone();
            let this = self.get_mut();
            unsafe {
                *this.inner.waker.get() = Some(waker);
            }

            Poll::Pending
        } else if state == 2 {
            // We've already received the value before.
            Poll::Ready(None)
        } else {
            panic!("unreachable")
        }
    }
}
