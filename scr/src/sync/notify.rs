use std::cell::{Cell, RefCell, UnsafeCell};
use std::future::Future;
use std::marker::PhantomPinned;
use std::mem;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::task::{Context, LocalWaker, Poll};

use crate::pin;
use crate::util::WakeList;
use crate::util::linked_list::{self, GuardedLinkedList, LinkedList};

#[derive(Debug)]
pub struct Notify {
    state: Cell<State>,
    waiters: RefCell<LinkedList<Waiter>>,
}

#[derive(Debug)]
struct Waiter {
    pointers: linked_list::Pointers<Waiter>,
    waker: UnsafeCell<Option<LocalWaker>>,
    notification: Cell<Option<Notification>>,
    _p: PhantomPinned,
}

impl Waiter {
    fn new() -> Self {
        Self {
            pointers: linked_list::Pointers::new(),
            waker: UnsafeCell::new(None),
            notification: Cell::new(None),
            _p: PhantomPinned,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyOneStrategy {
    Fifo,
    Lifo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notification {
    One(NotifyOneStrategy),
    All,
}

struct NotifyWaitersList<'a> {
    list: GuardedLinkedList<Waiter>,
    is_empty: bool,
    notify: &'a Notify,
}

impl<'a> NotifyWaitersList<'a> {
    fn new(
        unguarded_list: LinkedList<Waiter>,
        guard: Pin<&'a Waiter>,
        notify: &'a Notify,
    ) -> NotifyWaitersList<'a> {
        let guard_ptr = NonNull::from(guard.get_ref());
        let list = unguarded_list.into_guarded(guard_ptr);

        NotifyWaitersList {
            list,
            is_empty: false,
            notify,
        }
    }

    fn pop_back_locked(&mut self, _waiters: &mut LinkedList<Waiter>) -> Option<NonNull<Waiter>> {
        let result = self.list.pop_back();
        if result.is_none() {
            self.is_empty = true;
        }

        result
    }
}

impl Drop for NotifyWaitersList<'_> {
    fn drop(&mut self) {
        if !self.is_empty {
            let _lock_guard = self.notify.waiters.borrow_mut();
            while let Some(waiter) = self.list.pop_back() {
                let waiter = unsafe { waiter.as_ref() };
                waiter.notification.set(Some(Notification::All))
            }
        }
    }
}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Notified<'a> {
    notify: &'a Notify,
    state: NotifiedState,
    notify_waiters_calls: usize,
    waiter: Waiter,
}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct OwnedNotified {
    notify: Rc<Notify>,
    state: NotifiedState,
    notify_waiters_calls: usize,
    waiter: Waiter,
}

struct NotifiedProject<'a> {
    notify: &'a Notify,
    state: &'a mut NotifiedState,
    notify_waiters_calls: &'a usize,
    waiter: &'a Waiter,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    phase: Phase,
    notify_waiters_calls: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Empty,
    Waiting,
    Notified,
}

#[derive(Debug)]
enum NotifiedState {
    Init,
    Waiting,
    Done,
}

impl Notify {
    pub const fn new() -> Notify {
        Self {
            state: Cell::new(State {
                phase: Phase::Empty,
                notify_waiters_calls: 0,
            }),
            waiters: RefCell::new(LinkedList::new()),
        }
    }

    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            state: NotifiedState::Init,
            notify_waiters_calls: self.state.get().notify_waiters_calls,
            waiter: Waiter::new(),
        }
    }

    pub fn notified_owned(self: Rc<Self>) -> OwnedNotified {
        let notify_waiters_calls = self.state.get().notify_waiters_calls;

        OwnedNotified {
            notify: self,
            state: NotifiedState::Init,
            notify_waiters_calls,
            waiter: Waiter::new(),
        }
    }

    pub fn notify_one(&self) {
        self.notify_with_strategy(NotifyOneStrategy::Fifo);
    }

    pub fn notify_last(&self) {
        self.notify_with_strategy(NotifyOneStrategy::Lifo);
    }

    fn notify_with_strategy(&self, strategy: NotifyOneStrategy) {
        let mut curr = self.state.get();

        if matches!(curr.phase, Phase::Empty | Phase::Notified) {
            curr.phase = Phase::Notified;
            self.state.set(curr);
            return;
        }

        let mut waiters = self.waiters.borrow_mut();

        if let Some(waker) = notify_locked(&mut waiters, &self.state, curr, strategy) {
            drop(waiters);
            waker.wake();
        }
    }

    pub fn notify_waiters(&self) {
        self.lock_waiter_list().notify_waiters();
    }

    fn inner_notify_waiters<'a>(
        &'a self,
        mut curr: State,
        mut waiters: std::cell::RefMut<'a, LinkedList<Waiter>>,
    ) {
        if matches!(curr.phase, Phase::Empty | Phase::Notified) {
            curr.notify_waiters_calls += 1;
            self.state.set(curr);
            return;
        }

        self.state.set(State {
            phase: Phase::Empty,
            notify_waiters_calls: curr.notify_waiters_calls + 1,
        });

        let guard = Waiter::new();
        pin!(guard); // TODO: std pin?

        let mut list = NotifyWaitersList::new(std::mem::take(&mut *waiters), guard.as_ref(), self);

        let mut wakers = WakeList::new();
        'outer: loop {
            while wakers.can_push() {
                match list.pop_back_locked(&mut waiters) {
                    Some(waiter) => {
                        let waiter = unsafe { waiter.as_ref() };

                        if let Some(waker) = unsafe { (*waiter.waker.get()).take() } {
                            wakers.push(waker);
                        }

                        waiter.notification.set(Some(Notification::All))
                    }

                    None => {
                        break 'outer;
                    }
                }
            }

            drop(waiters);
            wakers.wake_all();

            waiters = self.waiters.borrow_mut();
        }

        drop(waiters);
        wakers.wake_all();
    }

    pub(crate) fn lock_waiter_list(&self) -> NotifyGuard<'_> {
        let guarded_waiters = self.waiters.borrow_mut();
        let current_state = self.state.get();

        NotifyGuard {
            guarded_notify: self,
            guarded_waiters,
            current_state,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl UnwindSafe for Notify {}
impl RefUnwindSafe for Notify {}

fn notify_locked(
    waiters: &mut LinkedList<Waiter>,
    state: &Cell<State>,
    curr: State,
    strategy: NotifyOneStrategy,
) -> Option<LocalWaker> {
    match curr.phase {
        Phase::Empty | Phase::Notified => {
            state.set(State {
                phase: Phase::Notified,
                notify_waiters_calls: curr.notify_waiters_calls,
            });

            None
        }

        Phase::Waiting => {
            let waiter = match strategy {
                NotifyOneStrategy::Fifo => waiters.pop_back().unwrap(),
                NotifyOneStrategy::Lifo => waiters.pop_front().unwrap(),
            };

            let waiter = unsafe { waiter.as_ref() };
            let waker = unsafe { (*waiter.waker.get()).take() };
            waiter.notification.set(Some(Notification::One(strategy)));

            if waiters.is_empty() {
                state.set(State {
                    phase: Phase::Empty,
                    notify_waiters_calls: curr.notify_waiters_calls,
                });
            }

            waker
        }
    }
}

impl Notified<'_> {
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.poll_notified(None).is_ready()
    }

    fn project(self: Pin<&mut Self>) -> NotifiedProject<'_> {
        unsafe {
            is_unpin::<&Notify>();
            is_unpin::<NotifiedState>();
            is_unpin::<usize>();

            let me = self.get_unchecked_mut();
            NotifiedProject {
                notify: me.notify,
                state: &mut me.state,
                notify_waiters_calls: &me.notify_waiters_calls,
                waiter: &me.waiter,
            }
        }
    }

    fn poll_notified(self: Pin<&mut Self>, waker: Option<&LocalWaker>) -> Poll<()> {
        self.project().poll_notified(waker)
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_notified(Some(cx.local_waker()))
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        unsafe { Pin::new_unchecked(self) }
            .project()
            .drop_notified();
    }
}

impl OwnedNotified {
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.poll_notified(None).is_ready()
    }

    fn project(self: Pin<&mut Self>) -> NotifiedProject<'_> {
        unsafe {
            is_unpin::<&Notify>();
            is_unpin::<NotifiedState>();
            is_unpin::<usize>();

            let me = self.get_unchecked_mut();
            NotifiedProject {
                notify: &me.notify,
                state: &mut me.state,
                notify_waiters_calls: &me.notify_waiters_calls,
                waiter: &me.waiter,
            }
        }
    }

    fn poll_notified(self: Pin<&mut Self>, waker: Option<&LocalWaker>) -> Poll<()> {
        self.project().poll_notified(waker)
    }
}

impl Future for OwnedNotified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_notified(Some(cx.local_waker()))
    }
}

impl Drop for OwnedNotified {
    fn drop(&mut self) {
        unsafe { Pin::new_unchecked(self) }
            .project()
            .drop_notified();
    }
}

impl NotifiedProject<'_> {
    fn poll_notified(self, waker: Option<&LocalWaker>) -> Poll<()> {
        let NotifiedProject {
            notify,
            state,
            notify_waiters_calls,
            waiter,
        } = self;

        match *state {
            NotifiedState::Init => {
                let curr = notify.state.get();

                if curr.notify_waiters_calls != *notify_waiters_calls {
                    *state = NotifiedState::Done;
                    return Poll::Ready(());
                }

                if matches!(curr.phase, Phase::Notified) {
                    notify.state.set(State {
                        phase: Phase::Empty,
                        ..curr
                    });
                    *state = NotifiedState::Done;
                    return Poll::Ready(());
                }

                let waker = waker.cloned();
                let curr = notify.state.get();
                let mut waiters = notify.waiters.borrow_mut();

                if curr.notify_waiters_calls != *notify_waiters_calls {
                    *state = NotifiedState::Done;
                    return Poll::Ready(());
                }

                match curr.phase {
                    Phase::Waiting => (),
                    Phase::Empty => notify.state.set(State {
                        phase: Phase::Waiting,
                        ..curr
                    }),
                    Phase::Notified => {
                        notify.state.set(State {
                            phase: Phase::Empty,
                            ..curr
                        });

                        *state = NotifiedState::Done;
                        return Poll::Ready(());
                    }
                }

                let mut old_waker = None;
                if waker.is_some() {
                    unsafe {
                        old_waker = mem::replace(&mut *waiter.waker.get(), waker);
                    }
                }

                waiters.push_front(NonNull::from(waiter));
                *state = NotifiedState::Waiting;

                drop(waiters);
                drop(old_waker);

                Poll::Pending
            }

            NotifiedState::Waiting => {
                let mut old_waker = None;
                let mut waiters = notify.waiters.borrow_mut();

                if waiter.notification.get().is_some() {
                    old_waker = unsafe { (*waiter.waker.get()).take() };
                    waiter.notification.set(None);

                    drop(waiters);
                    drop(old_waker);

                    *state = NotifiedState::Done;
                    return Poll::Ready(());
                }

                let curr = notify.state.get();
                if curr.notify_waiters_calls != *notify_waiters_calls {
                    old_waker = unsafe { (*waiter.waker.get()).take() };
                    unsafe { waiters.remove(NonNull::from(waiter)) };
                    *state = NotifiedState::Done;

                    drop(waiters);
                    drop(old_waker);

                    return Poll::Ready(());
                }

                unsafe {
                    let v = &mut *waiter.waker.get();
                    if let Some(waker) = waker {
                        let should_update = match v {
                            Some(current_waker) => !current_waker.will_wake(waker),
                            None => true,
                        };

                        if should_update {
                            old_waker = v.replace(waker.clone());
                        }
                    }
                }

                drop(waiters);
                drop(old_waker);

                Poll::Pending
            }

            NotifiedState::Done => Poll::Ready(()),
        }
    }

    fn drop_notified(self) {
        let NotifiedProject {
            notify,
            state,
            waiter,
            ..
        } = self;

        if matches!(*state, NotifiedState::Waiting) {
            let mut waiters = notify.waiters.borrow_mut();
            let mut notify_state = notify.state.get();
            let notification = waiter.notification.get();

            unsafe { waiters.remove(NonNull::from(waiter)) };

            if waiters.is_empty() && matches!(notify_state.phase, Phase::Waiting) {
                notify_state.phase = Phase::Empty;
                notify.state.set(notify_state);
            }

            if let Some(Notification::One(strategy)) = notification
                && let Some(waker) =
                    notify_locked(&mut waiters, &notify.state, notify_state, strategy)
            {
                drop(waiters);
                waker.wake();
            }
        }
    }
}

unsafe impl linked_list::Link for Waiter {
    type Handle = NonNull<Waiter>;
    type Target = Waiter;

    fn as_raw(handle: &NonNull<Waiter>) -> NonNull<Waiter> {
        *handle
    }

    unsafe fn from_raw(ptr: NonNull<Waiter>) -> NonNull<Waiter> {
        ptr
    }

    unsafe fn pointers(target: NonNull<Waiter>) -> NonNull<linked_list::Pointers<Waiter>> {
        let r = unsafe { &target.as_ref().pointers };
        NonNull::from_ref(r)
    }
}

fn is_unpin<T: Unpin>() {}

pub(crate) struct NotifyGuard<'a> {
    guarded_notify: &'a Notify,
    guarded_waiters: std::cell::RefMut<'a, LinkedList<Waiter>>,
    current_state: State,
}

impl NotifyGuard<'_> {
    pub(crate) fn notify_waiters(self) {
        self.guarded_notify
            .inner_notify_waiters(self.current_state, self.guarded_waiters);
    }
}
