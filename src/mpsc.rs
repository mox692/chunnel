/// TODO:
/// * remove mutex and use cas-loop for updating a head / tail
///
///
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::{Arc, Mutex},
};

struct Inner<T, const N: usize> {
    bucket: UnsafeCell<MaybeUninit<[T; N]>>,
    /// A place next sender will write to.
    head: usize,
    /// A place receiver will read from.
    tail: usize,
}

fn channel<T, const N: usize>() -> (Tx<T, N>, Rx<T, N>) {
    let inner = Arc::new(Mutex::new(Inner {
        bucket: UnsafeCell::new(MaybeUninit::uninit()),
        head: 0,
        tail: 0,
    }));
    let tx = Tx {
        inner: inner.clone(),
    };
    let rx = Rx { inner };

    (tx, rx)
}

struct Tx<T, const N: usize> {
    inner: Arc<Mutex<Inner<T, N>>>,
}

struct Rx<T, const N: usize> {
    inner: Arc<Mutex<Inner<T, N>>>,
}

impl<T, const N: usize> Tx<T, N> {
    fn send(&self, v: T) {
        let guard = self.inner.lock().unwrap();
        let head = guard.head;

        if N <= head {
            panic!("sender is full!")
        }

        let ptr = guard.bucket.get().wrapping_add(head + 1) as *mut T;
        unsafe { ptr.write(v) };
        drop(guard)
    }
}

impl<T, const N: usize> Rx<T, N> {
    async fn recv(&mut self) -> Option<T> {
        None
    }
}

impl<T, const N: usize> Clone for Tx<T, N> {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        Tx { inner }
    }
}
