#[cfg(not(all(loom, test)))]
pub(crate) type Mutex<T> = std::sync::Mutex<T>;
#[cfg(all(loom, test))]
pub(crate) type Mutex<T> = loom::sync::Mutex<T>;

#[cfg(not(all(loom, test)))]
pub(crate) type Arc<T> = std::sync::Arc<T>;
#[cfg(all(loom, test))]
pub(crate) type Arc<T> = loom::sync::Arc<T>;

#[cfg(not(all(loom, test)))]
#[derive(Debug)]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

#[cfg(not(all(loom, test)))]
impl<T> UnsafeCell<T> {
    pub(crate) const fn new(data: T) -> UnsafeCell<T> {
        UnsafeCell(std::cell::UnsafeCell::new(data))
    }

    #[inline(always)]
    pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    #[inline(always)]
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}

#[cfg(all(loom, test))]
pub(crate) type UnsafeCell<T> = loom::cell::UnsafeCell<T>;

#[cfg(not(all(loom, test)))]
pub(crate) type AtomicUsize = std::sync::atomic::AtomicUsize;
#[cfg(all(loom, test))]
pub(crate) type AtomicUsize = loom::sync::atomic::AtomicUsize;

#[cfg(not(all(loom, test)))]
pub(crate) type AtomicBool = std::sync::atomic::AtomicBool;
#[cfg(all(loom, test))]
pub(crate) type AtomicBool = loom::sync::atomic::AtomicBool;
