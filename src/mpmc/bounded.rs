//! Array based bounded async mpmc channel.

/// Optimization ideas:
/// · batch insert / delete
/// · use lock-free linked list for the waiter
/// · fast-path, slow-path
use crate::loom_wrapper::{Arc, AtomicUsize, Mutex, UnsafeCell};
use std::future::Future;
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::SeqCst;
use std::task::{Context, Poll, Waker};

///  rx                   tx
///   |                   |
///  [ ]   -   [ ]   -   [ ]    -> ...
///  tail                head
struct Inner<T, const N: usize> {
    bucket: Box<[Bucket<T>]>,

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

    /// Wait list of `Rx`
    rx_waker_linked_list_handle: Mutex<LinkedListHandle<WaitNode>>,

    /// Wait list of `Tx`
    tx_waker_linked_list_handle: Mutex<LinkedListHandle<WaitNode>>,
}

struct LinkedListHandle<NodeType> {
    head: Option<NonNull<NodeType>>,
    tail: Option<NonNull<NodeType>>,
}

struct Bucket<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    stamp: AtomicUsize,
}

impl<T, const N: usize> Inner<T, N> {
    fn write_at(&self, index: usize, v: T) {
        debug_assert!(index < N);

        // SAFETY: Index must not be out of range (not greater than N).
        self.bucket[index]
            .data
            // TODO: loom says that there is a concurrent read during this with_mut operation.
            // i.e. read may not see this latest value
            .with_mut(|ptr| unsafe { (ptr as *mut T).write(v) });
    }

    fn read_at(&self, index: usize) -> T {
        debug_assert!(index < N);

        // SAFETY: Index must not be out of range (not greater than N).
        self.bucket[index]
            .data
            .with(|ptr| unsafe { ptr.read().assume_init() })
    }

    // If passed a Rx packed value, then check if all rxes are closed (i.e. dropped )or not.
    #[inline(always)]
    fn is_closed(&self, packed: usize) -> bool {
        let closed_bit = self.get_closed_bit();
        closed_bit & packed == closed_bit
    }

    #[inline(always)]
    fn one_lap(&self) -> usize {
        self.carry_up_next_power_of_two() << 1
    }

    #[inline(always)]
    fn get_closed_bit(&self) -> usize {
        self.carry_up_next_power_of_two()
    }

    #[inline(always)]
    fn carry_up_next_power_of_two(&self) -> usize {
        if N == N.next_power_of_two() {
            (N + 1).next_power_of_two()
        } else {
            N.next_power_of_two()
        }
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

    fn get_stamp(&self, index: usize) -> usize {
        debug_assert!(index < N);
        self.bucket[index].stamp.load(SeqCst)
    }

    /// If this sender can acquire a position to write
    /// then this function returns Ok(pos), otherwise Err(_).
    fn start_send(&self) -> Result<usize, TxError> {
        let mut cur_head = self.head.load(SeqCst);

        loop {
            if self.is_closed(cur_head) {
                return Err(TxError::Closed);
            }
            // If there still be a space, then try to send a value
            let stamp = self.get_stamp(self.get_buf_index(cur_head));
            if stamp < cur_head {
                return Err(TxError::Full);
            } else if cur_head < stamp {
                cur_head = self.head.load(SeqCst);
                continue;
            }

            // If there still be a space, then try to send a value
            let cur_head_pos = self.get_buf_index(cur_head);

            let next = if cur_head_pos + 1 == N {
                // next lap
                (cur_head + self.one_lap()) & !(self.one_lap() - 1)
            } else {
                cur_head_pos + 1
            };

            // try to update head
            match self
                .head
                .compare_exchange_weak(cur_head, next, SeqCst, SeqCst)
            {
                Ok(_) => {
                    return Ok(cur_head_pos);
                }
                Err(now) => cur_head = now,
            }
        }
    }

    /// If this receiver can acquire a position to read
    /// then this function returns Ok(pos), otherwise Err(_).
    fn start_recv(&self) -> Result<usize, RxError> {
        let mut cur_tail = self.tail.load(SeqCst);

        loop {
            let cur_tail_pos = self.get_buf_index(cur_tail);

            let stamp = self.get_stamp(self.get_buf_index(cur_tail));
            if stamp < cur_tail + 1 {
                return Err(RxError::NoValueToRead);
            } else if cur_tail + 1 < stamp {
                cur_tail = self.tail.load(SeqCst);
                continue;
            }

            let next_pos = if cur_tail_pos + 1 == N {
                // next lap
                (cur_tail_pos + self.one_lap()) & !(self.one_lap() - 1)
            } else {
                cur_tail_pos + 1
            };

            let next = self.pack_pos(next_pos, cur_tail);

            match self
                .tail
                .compare_exchange_weak(cur_tail, next, SeqCst, SeqCst)
            {
                Ok(_) => {
                    return Ok(cur_tail_pos);
                }
                Err(now) => cur_tail = now,
            }
        }
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
            // SAFETY: Index must not be out of range (not greater than N).
            let v = self.bucket[i]
                .data
                .with(|ptr| unsafe { ptr.read().assume_init() });
            drop(v);
        }
    }
}

/// docs
pub fn bounded<T, const N: usize>() -> (Tx<T, N>, Rx<T, N>) {
    assert!(1 <= N, "Capacity must be greater than 1");

    let inner = Arc::new(Inner {
        bucket: (0..N)
            .map(|i| Bucket {
                data: UnsafeCell::new(MaybeUninit::uninit()),
                stamp: AtomicUsize::new(i),
            })
            .collect(),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        tx_count: AtomicUsize::new(1),
        rx_count: AtomicUsize::new(1),
        rx_waker_linked_list_handle: Mutex::new(LinkedListHandle {
            head: None,
            tail: None,
        }),
        tx_waker_linked_list_handle: Mutex::new(LinkedListHandle {
            head: None,
            tail: None,
        }),
    });

    let tx = Tx {
        inner: inner.clone(),
    };
    let rx = Rx { inner };

    (tx, rx)
}

/// SAFETY:
/// `T` needs to be Send in order to make Tx<T, N> Send since otherwise
/// we could send a non-Send value to another thread by using `Tx::send()`
/// and `Rx::recv()`.
///
/// Whether T implements Sync or not does not matter because currently we don't
/// provide a api that enables to send a `&T` to multiple threads.
unsafe impl<T: Send, const N: usize> Send for Tx<T, N> {}
unsafe impl<T: Send, const N: usize> Send for Rx<T, N> {}

/// SAFETY:
/// Since we provide `Tx::send(&self)` api and `Rx::recv(&self)` api, just having
/// shared reference is enough for `Tx` or `Rx` to send a value to another thread.
/// This means that a shared access to the `Tx` or `Rx` across multiple threads
/// should be safe only when T implement Send.
///
/// Whether T implements Sync or not does not matter because currently we don't
/// provide a api that enables to send a `&T` to multiple threads.
unsafe impl<T: Send, const N: usize> Sync for Tx<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for Rx<T, N> {}

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

/// This struct is `Unpin`
struct TxRef<'a, T, const N: usize> {
    inner: &'a Tx<T, N>,
    val: Option<T>,
    wait_node: WaitNode,
}

/// Actual linked-list node
struct WaitNode {
    next: Option<NonNull<Self>>,
    waker: Option<Waker>,
    _p: PhantomPinned,
}

impl<'a, T, const N: usize> Future for TxRef<'a, T, N> {
    type Output = Result<(), TxError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        {
            let res = self.inner.inner.start_send();
            match res {
                Err(TxError::Full) => {
                    let waker = cx.waker().clone();

                    // construct linked list node
                    // SAFETY: `TxRef` is !Unpin, so it should not be moved.
                    let this = unsafe { self.get_unchecked_mut() };
                    this.wait_node.waker = Some(waker);
                    let node = &mut this.wait_node;

                    let mut linked_list_guard =
                        this.inner.inner.tx_waker_linked_list_handle.lock().unwrap();

                    if linked_list_guard.tail.is_some() {
                        unsafe {
                            linked_list_guard.tail.unwrap().as_mut().next =
                                Some(NonNull::new_unchecked(node))
                        }
                    } else {
                        assert!(linked_list_guard.head.is_none());

                        linked_list_guard.head = unsafe { Some(NonNull::new_unchecked(node)) };
                        linked_list_guard.tail = linked_list_guard.head;
                    }

                    drop(linked_list_guard);

                    return Poll::Pending;
                }
                Err(TxError::Closed) => return Poll::Ready(Err(TxError::Closed)),
                Ok(pos) => {
                    debug_assert!(pos < N);

                    // SAFETY: `TxRef` is `!Unpin`, so it is guarantee that `Self` won't move.
                    let this = unsafe { self.get_unchecked_mut() };
                    let v = this.val.take().expect("inner value should not be None");
                    this.inner.inner.write_at(pos, v);

                    // It is gagaranteed that a Rx that see this updated stamp must also see the latest
                    // written value `v`.
                    this.inner.inner.bucket[pos].stamp.fetch_add(1, SeqCst);

                    let mut guard = this.inner.inner.rx_waker_linked_list_handle.lock().unwrap();
                    if guard.head.is_some() {
                        if guard.tail.is_some() {
                            if guard.tail.unwrap().as_ptr() == guard.head.unwrap().as_ptr() {
                                // only one element
                                let mut cur_head = guard.head.take().unwrap();
                                let waker = unsafe { cur_head.as_mut().waker.take().unwrap() };
                                waker.wake();
                                guard.head = None;
                                guard.tail = None;
                            } else {
                                let mut cur_head = guard.head.take().unwrap();
                                let waker = unsafe { cur_head.as_mut().waker.take().unwrap() };
                                waker.wake();
                                let next = unsafe { cur_head.as_mut().next };
                                guard.head = next;
                            }
                        } else {
                            panic!("This should not happen!!")
                        }
                    }
                    drop(guard);

                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

/// This struct is `Unpin`
struct RxRef<'a, T, const N: usize> {
    inner: &'a Rx<T, N>,
    wait_node: WaitNode,
}

impl<'a, T, const N: usize> Future for RxRef<'a, T, N> {
    type Output = Result<T, RxError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.inner.inner.start_recv() {
                Err(RxError::NoValueToRead) => {
                    let waker = cx.waker().clone();

                    let mut linked_list_guard =
                        self.inner.inner.rx_waker_linked_list_handle.lock().unwrap();

                    // check if just inserted?
                    let cur_head = self.inner.inner.head.load(SeqCst);
                    let cur_tail = self.inner.inner.tail.load(SeqCst);
                    if cur_tail < cur_head {
                        drop(linked_list_guard);
                        // This is required for loom tests, since in theory there are some scenarios where
                        // Tx's CAS succeeded but stamp has not yet updated. In those cases, a thread trying
                        // to read value by a Rx could run into infinite loop.
                        #[cfg(all(loom, test))]
                        crate::loom_wrapper::yield_now();

                        continue;
                    }

                    let this = unsafe { self.get_unchecked_mut() };
                    this.wait_node.waker = Some(waker);
                    let node = &mut this.wait_node;

                    let new_tail = unsafe { Some(NonNull::new_unchecked(node)) };

                    // If this RxRef has called `poll()` multiple times, `node.next` could have Some(node).
                    node.next = None;
                    let cur_tail = linked_list_guard.tail;
                    if cur_tail.is_some() {
                        unsafe { cur_tail.unwrap().as_mut().next = new_tail };
                        linked_list_guard.tail = new_tail;
                    } else {
                        linked_list_guard.head = new_tail;
                        linked_list_guard.tail = new_tail;
                    }

                    drop(linked_list_guard);

                    return Poll::Pending;
                }
                Err(RxError::Closed) => return Poll::Ready(Err(RxError::Closed)),
                Ok(pos) => {
                    // read at cur_tail value! (not next)
                    let next = pos + self.inner.inner.one_lap();
                    let res = self.inner.inner.read_at(pos);
                    self.inner.inner.bucket[pos].stamp.store(next, SeqCst);

                    // If there is a waiting `Tx` task, then wake it up
                    let mut guard = self.inner.inner.tx_waker_linked_list_handle.lock().unwrap();
                    if guard.head.is_some() {
                        let mut node = guard.head.take().unwrap();
                        let waker = unsafe { node.as_mut().waker.take().unwrap() };
                        waker.wake();
                        let next = unsafe { node.as_mut().next };
                        guard.head = next;
                    }
                    drop(guard);

                    return Poll::Ready(Ok(res));
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
            wait_node: WaitNode {
                next: None,
                waker: None,
                _p: PhantomPinned,
            },
        };

        // `TxRef` is pinned here
        fut.await
    }

    pub fn try_send(&self, v: T) -> Result<(), TxError> {
        match self.inner.start_send() {
            Err(e) => Err(e),
            Ok(pos) => {
                debug_assert!(pos < N);

                self.inner.write_at(pos, v);
                // It is gagaranteed that a Rx that see this updated stamp must also see the latest
                // written value `v`.
                self.inner.bucket[pos].stamp.fetch_add(1, SeqCst);

                // // If there is a waiting `Rx` task, then wake it up
                let mut rx_linked_list_guard =
                    self.inner.rx_waker_linked_list_handle.lock().unwrap();

                if rx_linked_list_guard.head.is_some() {
                    let mut node = rx_linked_list_guard.head.take().unwrap();
                    let waker = unsafe { node.as_mut().waker.take().unwrap() };
                    waker.wake();
                    let next = unsafe { node.as_mut().next };
                    rx_linked_list_guard.head = next;
                }
                drop(rx_linked_list_guard);

                return Ok(());
            }
        }
    }
}

impl<T, const N: usize> Rx<T, N> {
    pub async fn recv(&self) -> Result<T, RxError> {
        let rx_ref = RxRef {
            inner: self,
            wait_node: WaitNode {
                next: None,
                waker: None,
                _p: PhantomPinned,
            },
        };
        rx_ref.await
    }

    /// Try to receive a value from the queue.
    ///
    /// If there is no value available, then returns `Err(RxError::NoValue)`,
    /// otherwise returns a value.
    ///
    /// Note that this api returns `RxError::NoValue` even if all senders are
    /// dropped and there is no value available.
    pub fn try_recv(&self) -> Result<T, RxError> {
        match self.inner.start_recv() {
            Err(e) => Err(e),
            Ok(pos) => {
                // read at cur_tail value! (not next)
                let next = pos + self.inner.one_lap();
                let res = self.inner.read_at(pos);
                self.inner.bucket[pos].stamp.store(next, SeqCst);

                // If there is a waiting `Tx` task, then wake it up
                let mut tx_linked_list_guard =
                    self.inner.tx_waker_linked_list_handle.lock().unwrap();

                if tx_linked_list_guard.head.is_some() {
                    let mut node = tx_linked_list_guard.head.take().unwrap();
                    let waker = unsafe { node.as_mut().waker.take().unwrap() };
                    waker.wake();
                    let next = unsafe { node.as_mut().next };
                    tx_linked_list_guard.head = next;
                }

                Ok(res)
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

            // TODO: refactor
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

#[cfg(all(test, loom))]
mod loom_test {
    use std::sync::mpsc::TryRecvError;

    use super::*;
    use loom::thread::{self, yield_now};
    use tokio_test::{assert_pending, assert_ready, assert_ready_ok, task};

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
            assert_eq!(rx.try_recv(), Err(RxError::NoValueToRead))
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
    fn max_cap() {
        loom::model(|| {
            let (tx, _rx) = bounded::<i32, 1>();
            assert_eq!(tx.try_send(0), Ok(()));
            assert_eq!(tx.try_send(0), Err(TxError::Full));
        });
    }

    #[test]
    fn send_wake_up() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 1>();

            let mut task = task::spawn(tx.send(1));
            assert_ready_ok!(task.poll());

            let mut task = task::spawn(tx.send(2));
            assert_pending!(task.poll());

            assert_eq!(rx.try_recv(), Ok(1));
            assert_ready_ok!(task.poll());
            assert_eq!(rx.try_recv(), Ok(2));
        });
    }

    #[test]
    fn recv_wake_up() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 1>();

            let mut rx_task = task::spawn(rx.recv());
            assert_pending!(rx_task.poll());

            let mut tx_task = task::spawn(tx.send(1));
            assert_ready_ok!(tx_task.poll());

            assert_ready_ok!(rx_task.poll());
        });
    }

    #[test]
    fn send_wake_up_multi_thread() {
        loom::model(|| {
            let (tx, rx) = bounded::<i32, 2>();

            let jh = loom::thread::spawn(move || {
                loom::future::block_on(async {
                    let v = rx.recv().await.unwrap();

                    assert_eq!(v, 1);
                });
            });

            loom::future::block_on(async {
                tx.send(1).await.unwrap();
            });

            jh.join().unwrap()
        });
    }

    // It takes 10 minutes or so
    #[test]
    fn multi_threads_send_recv() {
        loom::model(|| {
            let num_task = 1;
            let msg_per_task = 1;
            let num_threads = 2;

            let (tx, rx) = bounded::<i32, 10>();

            let mut jhs = vec![];
            for _ in 0..num_threads {
                let tx = tx.clone();
                let jh = loom::thread::spawn(move || {
                    for i in 0..num_task {
                        loom::future::block_on(async {
                            for j in 0..msg_per_task {
                                tx.try_send(j + msg_per_task * i).unwrap();
                            }
                        });
                    }
                });
                jhs.push(jh);
            }

            loom::future::block_on(async {
                let mut num_msg = 0;
                loop {
                    if let Ok(_) = rx.try_recv() {
                        num_msg += 1;
                    }
                    if num_task * msg_per_task <= num_msg {
                        break;
                    }

                    yield_now();
                }
            });

            for jh in jhs.drain(..) {
                jh.join().unwrap();
            }
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
