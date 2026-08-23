//! A thin wrapper around [`std::cell::UnsafeCell`].
//!
//! This mirrors the API that `tokio` exposes through its `loom` shim so that
//! code translated from `tokio` keeps the `with` / `with_mut` shape. Since this
//! runtime is single threaded there is no `loom` variant; the wrapper compiles
//! away entirely.

#[derive(Debug)]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

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

    #[inline(always)]
    pub(crate) fn get(&self) -> *mut T {
        self.0.get()
    }
}
