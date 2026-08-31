use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::{mem, ops};

//use tokio_stream::Stream;

pub fn spawn<T>(task: T) -> Spawn<T> {
    Spawn {
        task: MockTask::new(),
        future: Box::pin(task),
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Spawn<T> {
    task: MockTask,
    future: Pin<Box<T>>,
}

#[derive(Debug, Clone)]
struct MockTask {
    waker: Arc<ThreadWaker>,
}

#[derive(Debug)]
struct ThreadWaker {
    state: Mutex<usize>,
    condvar: Condvar,
}

const IDLE: usize = 0;
const WAKE: usize = 1;
const SLEEP: usize = 2;

const POLL_UNTIL_IDLE_MAX_ITERATIONS: usize = 150;

impl<T> Spawn<T> {
    pub fn into_inner(self) -> T
    where
        T: Unpin,
    {
        *Pin::into_inner(self.future)
    }

    pub fn is_woken(&self) -> bool {
        self.task.is_woken()
    }

    pub fn waker_ref_count(&self) -> usize {
        self.task.waker_ref_count()
    }

    pub fn enter<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Context<'_>, Pin<&mut T>) -> R,
    {
        let fut = self.future.as_mut();
        self.task.enter(|cx| f(cx, fut))
    }
}

impl<T: Unpin> ops::Deref for Spawn<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.future
    }
}

impl<T: Unpin> ops::DerefMut for Spawn<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.future
    }
}

impl<T: Future> Spawn<T> {
    pub fn poll(&mut self) -> Poll<T::Output> {
        let fut = self.future.as_mut();
        self.task.enter(|cx| fut.poll(cx))
    }

    pub fn poll_until_idle(&mut self) -> Poll<T::Output> {
        for _ in 0..POLL_UNTIL_IDLE_MAX_ITERATIONS {
            let result = self.poll();
            if result.is_ready() || !self.is_woken() {
                return result;
            }
        }
        panic!(
            "poll_until_idle exceeded {POLL_UNTIL_IDLE_MAX_ITERATIONS} iterations; future may be waking without making progress"
        );
    }
}

//impl<T: Stream> Spawn<T> {
//    /// If `T` is a [`Stream`] then `poll_next` it. This will handle pinning and the context
//    /// type for the stream.
//    pub fn poll_next(&mut self) -> Poll<Option<T::Item>> {
//        let stream = self.future.as_mut();
//        self.task.enter(|cx| stream.poll_next(cx))
//    }
//}

impl<T: Future> Future for Spawn<T> {
    type Output = T::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

//impl<T: Stream> Stream for Spawn<T> {
//    type Item = T::Item;
//
//    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//        self.future.as_mut().poll_next(cx)
//    }
//
//    fn size_hint(&self) -> (usize, Option<usize>) {
//        self.future.size_hint()
//    }
//}

impl MockTask {
    /// Creates new mock task
    fn new() -> Self {
        MockTask {
            waker: Arc::new(ThreadWaker::new()),
        }
    }

    fn enter<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Context<'_>) -> R,
    {
        self.waker.clear();
        let waker = self.waker();
        let mut cx = Context::from_waker(&waker);

        f(&mut cx)
    }

    fn is_woken(&self) -> bool {
        self.waker.is_woken()
    }

    fn waker_ref_count(&self) -> usize {
        Arc::strong_count(&self.waker)
    }

    fn waker(&self) -> Waker {
        unsafe {
            let raw = to_raw(self.waker.clone());
            Waker::from_raw(raw)
        }
    }
}

impl Default for MockTask {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadWaker {
    fn new() -> Self {
        ThreadWaker {
            state: Mutex::new(IDLE),
            condvar: Condvar::new(),
        }
    }

    fn clear(&self) {
        *self.state.lock().unwrap() = IDLE;
    }

    fn is_woken(&self) -> bool {
        match *self.state.lock().unwrap() {
            IDLE => false,
            WAKE => true,
            _ => unreachable!(),
        }
    }

    fn wake(&self) {
        let mut state = self.state.lock().unwrap();
        let prev = *state;

        if prev == WAKE {
            return;
        }

        *state = WAKE;

        if prev == IDLE {
            return;
        }

        assert_eq!(prev, SLEEP);
        self.condvar.notify_one();
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

unsafe fn to_raw(waker: Arc<ThreadWaker>) -> RawWaker {
    RawWaker::new(Arc::into_raw(waker) as *const (), &VTABLE)
}

unsafe fn from_raw(raw: *const ()) -> Arc<ThreadWaker> {
    unsafe { Arc::from_raw(raw as *const ThreadWaker) }
}

unsafe fn clone(raw: *const ()) -> RawWaker {
    let waker = unsafe { from_raw(raw) };

    mem::forget(waker.clone());

    unsafe { to_raw(waker) }
}

unsafe fn wake(raw: *const ()) {
    let waker = unsafe { from_raw(raw) };
    waker.wake();
}

unsafe fn wake_by_ref(raw: *const ()) {
    let waker = unsafe { from_raw(raw) };
    waker.wake();

    mem::forget(waker);
}

unsafe fn drop_waker(raw: *const ()) {
    let _ = unsafe { from_raw(raw) };
}
