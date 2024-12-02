//! Array based bounded async mpmc channel.

use crate::loom_wrapper::{Arc, AtomicUsize, Mutex, UnsafeCell};
use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::task::{Poll, Waker};

///  rx                   tx
///   |                   |
///  [ ]   -   [ ]   -   [ ]    -> ...
///  tail                head
struct Inner<T, const N: usize> {
    bucket: [UnsafeCell<MaybeUninit<T>>; N],

    /// A place next sender will write to.
    /// closed == 1 if all tx are dropped
    /// | lap (rest)  | closed(1 bit) | cap ( (log2 N) + 1 bit) |
    head: AtomicUsize,

    /// A place receiver will read from.
    /// closed = 1 if all rx are dropped
    /// | lap (rest)  | closed(1 bit) | cap ( (log2 N) + 1 bit) |
    tail: AtomicUsize,

    /// The number of Tx
    tx_count: AtomicUsize,

    /// The number of Rx
    rx_count: AtomicUsize,

    /// todo
    tx_wakers: Mutex<VecDeque<Waker>>,
}

impl<T, const N: usize> Inner<T, N> {
    fn write_at(&self, index: usize, v: T) {
        debug_assert!(index < N);
        // SAFETY: todo
        unsafe { (self.bucket.as_ptr() as *mut T).add(index).write(v) };
    }

    fn read_at(&self, index: usize) -> T {
        debug_assert!(index < N);
        // SAFETY: todo
        unsafe { (self.bucket.as_ptr() as *mut T).add(index).read() }
    }

    // If passed a Rx packed value, then check if all rxes are closed (i.e. dropped )or not.
    #[inline(always)]
    fn is_closed(&self, packed: usize) -> bool {
        let closed_bit = self.get_closed_bit();
        closed_bit & packed == closed_bit
    }

    // Return true if there are no spaces to write to due to the slow read by Rx.
    #[inline(always)]
    fn is_full(&self, packed_head: usize, packed_tail: usize) -> bool {
        let head_pos = self.get_buf_index(packed_head);
        let tail_pos = self.get_buf_index(packed_tail);

        if tail_pos <= head_pos {
            if head_pos - tail_pos == N - 1 {
                // head = N - 1, tail_pos = 0の時はfull
                true
            } else {
                false
            }
        } else {
            (packed_head + 1) - self.one_lap() == packed_tail
        }
    }

    #[inline(always)]
    fn one_lap(&self) -> usize {
        N.next_power_of_two() << 1
    }

    #[inline(always)]
    fn get_closed_bit(&self) -> usize {
        N.next_power_of_two()
    }

    #[inline(always)]
    fn cap(&self) -> usize {
        N
    }

    /// input:  10|1|01101
    /// output: 10|0|00000
    fn get_lap(packed: usize) -> usize {
        let one_lap = N.next_power_of_two();
        packed & !(one_lap - 1)
    }

    /// input:  10|1|01101
    /// output: 00|0|01101
    fn get_buf_index(&self, packed: usize) -> usize {
        let one_lap = N.next_power_of_two();
        packed & (one_lap - 1)
    }

    /// pos:    00|0|01101
    /// packed: 10|1|01100
    /// output: 10|1|01101
    fn pack_pos(&self, pos: usize, packed: usize) -> usize {
        pos | (!(N.next_power_of_two() - 1) & packed)
    }
}

impl<T, const N: usize> Drop for Inner<T, N> {
    // We have to manually drop the values that are not read by the receiver.
    fn drop(&mut self) {
        let cur_tail = self.tail.load(SeqCst);
        let cur_head = self.head.load(SeqCst);

        let cur_tail_pos = self.get_buf_index(cur_tail);
        let cur_head_pos = self.get_buf_index(cur_head);

        for i in cur_tail_pos..cur_head_pos {
            let v = unsafe { (self.bucket.as_ptr() as *mut T).add(i).read() };
            drop(v);
        }
    }
}

/// docs
pub fn bounded<T, const N: usize>() -> (Tx<T, N>, Rx<T, N>) {
    let inner = Arc::new(Inner {
        bucket: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        tx_count: AtomicUsize::new(1),
        rx_count: AtomicUsize::new(1),
        tx_wakers: Mutex::new(VecDeque::new()),
    });

    let tx = Tx {
        inner: inner.clone(),
    };
    let rx = Rx { inner };

    (tx, rx)
}

// TODO: check
unsafe impl<T: Send, const N: usize> Send for Tx<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for Tx<T, N> {}
unsafe impl<T: Send, const N: usize> Send for Rx<T, N> {}

#[derive(PartialEq, Eq, Debug)]
pub enum TxError {
    Full,
    Closed,
}
#[derive(PartialEq, Eq, Debug)]
pub enum RxError {
    NoValueToRead,
    Closed,
}

pub struct Tx<T, const N: usize> {
    inner: Arc<Inner<T, N>>,
}

pub struct Rx<T, const N: usize> {
    inner: Arc<Inner<T, N>>,
}

struct TxRef<'a, T, const N: usize> {
    inner: &'a Tx<T, N>,
    val: Option<T>,
    // TODO: why is this needed?
    _p: PhantomPinned,
}

// TODO: why does T: Unpin need?
impl<'a, T, const N: usize> Future for TxRef<'a, T, N> {
    type Output = Result<(), TxError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let cur_head = self.inner.inner.head.load(SeqCst);
        let cur_tail = self.inner.inner.tail.load(SeqCst);

        if self.inner.inner.is_closed(cur_tail) {
            return Poll::Ready(Err(TxError::Closed));
        }

        if self.inner.inner.is_full(cur_head, cur_tail) {
            // Push this TxRef into the wait list
            let waker = cx.waker().clone();
            let mut guard = self.inner.inner.tx_wakers.lock().unwrap();
            guard.push_back(waker);

            return Poll::Pending;
        }

        // If there still be a space, then try to send a value
        let mut cur_head = self.inner.inner.head.load(Relaxed);

        loop {
            let next = if cur_head + 1 == N {
                // next lap
                (cur_head + self.inner.inner.one_lap()) & !(self.inner.inner.one_lap() - 1)
            } else {
                cur_head + 1
            };

            // try to update head
            match self
                .inner
                .inner
                .head
                .compare_exchange_weak(cur_head, next, SeqCst, SeqCst)
            {
                Ok(_) => {
                    let index = self.inner.inner.get_buf_index(next);
                    // SAFETY: TODO
                    let this = unsafe { self.get_unchecked_mut() };
                    let v = this.val.take().expect("aaaaaaaaa");
                    this.inner.inner.write_at(index, v);

                    return Poll::Ready(Ok(()));
                }
                Err(now) => {
                    cur_head = now;
                }
            }
        }
    }
}

impl<T, const N: usize> Tx<T, N> {
    // suspended when buffer is full, and be woken up when
    // chan gets additional space.
    pub async fn send(&self, v: T) -> Result<(), TxError> {
        let fut = TxRef {
            inner: self,
            val: Some(v),
            _p: PhantomPinned,
        };
        fut.await
    }

    pub fn try_send(&self, v: T) -> Result<(), TxError> {
        let mut cur_head = self.inner.head.load(Relaxed);

        loop {
            let cur_tail = self.inner.tail.load(Relaxed);

            // Check if buffer is closed.
            if self.inner.is_closed(cur_tail) {
                return Err(TxError::Closed);
            }

            // Check if buffer if full.
            if self.inner.is_full(cur_head, cur_tail) {
                return Err(TxError::Full);
            }

            let next = if self.inner.get_buf_index(cur_head) + 1 == N {
                // next lap
                (cur_head + self.inner.one_lap()) & !(self.inner.one_lap() - 1)
            } else {
                cur_head + 1
            };

            // try to update head
            match self
                .inner
                .head
                .compare_exchange_weak(cur_head, next, SeqCst, SeqCst)
            {
                Ok(_) => {
                    // Write at *previous* pos.
                    let index = self.inner.get_buf_index(cur_head);

                    self.inner.write_at(index, v);

                    return Ok(());
                }
                Err(now) => {
                    cur_head = now;
                }
            }
        }
    }
}

impl<T, const N: usize> Rx<T, N> {
    pub async fn recv(&self) -> Option<T> {
        todo!()
    }
    pub fn try_recv(&self) -> Result<T, RxError> {
        let mut cur_tail = self.inner.tail.load(SeqCst);

        loop {
            let cur_tail_pos = self.inner.get_buf_index(cur_tail);
            let cur_head = self.inner.head.load(SeqCst);
            let cur_head_pos = self.inner.get_buf_index(cur_head);

            // Check if buffer is closed.
            if self.inner.is_closed(cur_head) {
                return Err(RxError::Closed);
            }

            // Is there a value to read?
            if cur_tail_pos == cur_head_pos {
                return Err(RxError::NoValueToRead);
            }

            let next_pos = if cur_tail_pos + 1 == N {
                // next lap
                (cur_tail_pos + self.inner.one_lap()) & !(self.inner.one_lap() - 1)
            } else {
                cur_tail_pos + 1
            };

            let next = self.inner.pack_pos(next_pos, cur_tail);

            match self
                .inner
                .tail
                .compare_exchange_weak(cur_tail, next, SeqCst, SeqCst)
            {
                Ok(_) => {
                    // read at cur_tail value! (not next)
                    return Ok(self.inner.read_at(cur_tail_pos));
                }
                Err(now) => cur_tail = now,
            }
        }
    }
}

impl<T, const N: usize> Clone for Tx<T, N> {
    fn clone(&self) -> Self {
        self.inner.tx_count.fetch_add(1, SeqCst);
        let inner = self.inner.clone();
        Tx { inner }
    }
}

impl<T, const N: usize> Drop for Tx<T, N> {
    fn drop(&mut self) {
        let mut cur_tx_count = self.inner.tx_count.load(SeqCst);

        while let Err(now) = self.inner.tx_count.compare_exchange_weak(
            cur_tx_count,
            cur_tx_count.saturating_sub(1),
            SeqCst,
            SeqCst,
        ) {
            cur_tx_count = now
        }

        if cur_tx_count == 1 {
            // This is the last Tx, so set Tx closed flag
            let mut cur_head = self.inner.head.load(SeqCst);
            let mut next = self.inner.get_closed_bit() | cur_head;
            while let Err(now) = self
                .inner
                .head
                .compare_exchange_weak(cur_head, next, SeqCst, SeqCst)
            {
                cur_head = now;
                next = self.inner.get_closed_bit() | cur_head;
            }
        }
    }
}

impl<T, const N: usize> Clone for Rx<T, N> {
    fn clone(&self) -> Self {
        self.inner.rx_count.fetch_add(1, SeqCst);
        let inner = self.inner.clone();
        Rx { inner }
    }
}

impl<T, const N: usize> Drop for Rx<T, N> {
    fn drop(&mut self) {
        let mut cur_rx_count = self.inner.rx_count.load(SeqCst);

        while let Err(now) = self.inner.rx_count.compare_exchange_weak(
            cur_rx_count,
            cur_rx_count.saturating_sub(1),
            SeqCst,
            SeqCst,
        ) {
            cur_rx_count = now
        }

        if cur_rx_count == 1 {
            // This is the last Rx, so set Rx closed flag
            let mut cur_tail = self.inner.tail.load(SeqCst);
            let mut next = self.inner.get_closed_bit() | cur_tail;
            while let Err(now) = self
                .inner
                .tail
                .compare_exchange_weak(cur_tail, next, SeqCst, SeqCst)
            {
                cur_tail = now;
                next = self.inner.get_closed_bit() | cur_tail;
            }
        }
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use std::sync::mpsc::TryRecvError;

    use super::*;
    use loom::thread;

    #[test]
    fn try_send_and_try_recv() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            tx.try_send(42).unwrap();
            assert_eq!(rx.try_recv().unwrap(), 42);
        });
    }

    #[test]
    fn try_send_and_try_recv_multi_thread() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            tx.try_send(42).unwrap();

            let jh = thread::spawn(move || {
                assert_eq!(rx.try_recv().unwrap(), 42);
            });
            jh.join().unwrap();
        });
    }

    #[test]
    fn drop_rx() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            drop(rx);
            assert_eq!(tx.try_send(42), Err(TxError::Closed))
        });
    }

    #[test]
    fn drop_tx() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            drop(tx);
            assert_eq!(rx.try_recv(), Err(RxError::Closed))
        });
    }

    #[test]
    fn clone_rx() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            let rx2 = rx.clone();
            drop(rx);
            tx.try_send(42).unwrap();
            assert_eq!(rx2.try_recv(), Ok(42));
        });
    }

    #[test]
    fn clone_drop_rx() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            let rx2 = rx.clone();
            let jh = thread::spawn(move || drop(rx2));
            tx.try_send(42).unwrap();
            assert_eq!(rx.try_recv(), Ok(42));
            jh.join().unwrap();
        });
    }

    #[test]
    fn send_multiple_value() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 10>();
            let rx2 = rx.clone();
            tx.try_send(42).unwrap();
            let jh = thread::spawn(move || {
                assert_eq!(rx2.try_recv(), Ok(42));
            });
            jh.join().unwrap();
            assert_eq!(rx.try_recv(), Err(RxError::NoValueToRead));

            tx.try_send(43).unwrap();
            assert_eq!(rx.try_recv(), Ok(43));
        });
    }

    #[test]
    fn inner_drop() {
        loom::model(|| {
            struct A(std::sync::mpsc::Sender<()>);
            impl Drop for A {
                fn drop(&mut self) {
                    self.0.send(()).unwrap();
                }
            }

            let (sender, receiver) = std::sync::mpsc::channel::<()>();

            {
                let a1 = A(sender.clone());
                let a2 = A(sender);

                let (tx, _rx) = bounded::<A, 10>();

                tx.try_send(a1).unwrap();
                tx.try_send(a2).unwrap();
            }

            assert_eq!(receiver.try_recv().unwrap(), ());
            assert_eq!(receiver.try_recv().unwrap(), ());
            assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
        });
    }
}
