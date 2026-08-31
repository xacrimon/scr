use std::cell::{Cell, RefCell, UnsafeCell};
use std::future::Future;
use std::marker::PhantomPinned;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::Ordering::{self, AcqRel, Acquire, Relaxed, Release};
use std::task::{Context, LocalWaker, Poll};
use std::{assert_matches, mem};

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
#[repr(usize)]
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
    state: State,
    notify_waiters_calls: usize,
    waiter: Waiter,
}

unsafe impl<'a> Send for Notified<'a> {}
unsafe impl<'a> Sync for Notified<'a> {}

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct OwnedNotified {
    notify: Rc<Notify>,
    state: State,
    notify_waiters_calls: usize,
    waiter: Waiter,
}

unsafe impl Sync for OwnedNotified {}

struct NotifiedProject<'a> {
    notify: &'a Notify,
    state: &'a mut State,
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
        let state = self.state.get();

        Notified {
            notify: &self,
            state,
            notify_waiters_calls: state.notify_waiters_calls,
            waiter: Waiter::new(),
        }
    }

    pub fn notified_owned(self: Rc<Self>) -> OwnedNotified {
        let state = self.state.get();

        OwnedNotified {
            notify: self,
            state,
            notify_waiters_calls: state.notify_waiters_calls,
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

        while matches!(curr.phase, Phase::Empty | Phase::Notified) {
            curr.phase = Phase::Notified;
            self.state.set(curr);
            return;
        }

        let mut waiters = self.waiters.borrow_mut();

        curr = self.state.get();

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

        let new_state = State {
            phase: Phase::Empty,
            notify_waiters_calls: curr.notify_waiters_calls + 1,
        };
        self.state.set(new_state);

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
            let new_state = State {
                phase: Phase::Notified,
                notify_waiters_calls: curr.notify_waiters_calls,
            };
            state.set(new_state);
            // TODO: can anything modify state since curr was read?
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
                let new_state = State {
                    phase: Phase::Empty,
                    notify_waiters_calls: curr.notify_waiters_calls,
                };
                state.set(new_state);
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
            is_unpin::<State>();
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
            is_unpin::<State>();
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

fn cell_cas<T>(cell: &Cell<T>, curr: T, new: T) -> Result<(), T>
where
    T: Copy + PartialEq,
{
    let v = cell.get();

    if curr != v {
        return Err(v);
    }

    cell.set(new);
    Ok(())
}

impl NotifiedProject<'_> {
    fn poll_notified(self, waker: Option<&LocalWaker>) -> Poll<()> {
        let NotifiedProject {
            notify,
            state,
            notify_waiters_calls,
            waiter,
        } = self;

        'outer_loop: loop {
            match state.phase {
                Phase::Empty => {
                    let curr = notify.state.get();

                    if curr.notify_waiters_calls != *notify_waiters_calls {
                        state.phase = Phase::Notified;
                        continue 'outer_loop;
                    }

                    //let mut ok = true;
                    //notify.state.update(|mut acc| {
                    //    if matches!(acc.phase, Phase::Notified) {
                    //        ok = false;
                    //        return acc;
                    //    }
                    //
                    //    acc.phase = Phase::Empty;
                    //    acc
                    //});
                    let cas_curr = State {
                        phase: Phase::Notified,
                        notify_waiters_calls: curr.notify_waiters_calls,
                    };
                    let cas_new = State {
                        phase: Phase::Empty,
                        notify_waiters_calls: curr.notify_waiters_calls,
                    };
                    let res = cell_cas(&notify.state, cas_curr, cas_new);

                    if res.is_ok() {
                        state.phase = Phase::Notified;
                        continue 'outer_loop;
                    }

                    let waker = waker.cloned();

                    let mut waiters = notify.waiters.borrow_mut();

                    let mut curr = notify.state.get();

                    if curr.notify_waiters_calls != *notify_waiters_calls {
                        state.phase = Phase::Notified;
                        continue 'outer_loop;
                    }

                    loop {
                        match curr.phase {
                            Phase::Empty => {
                                let cas_curr = State {
                                    phase: Phase::Empty,
                                    notify_waiters_calls: curr.notify_waiters_calls,
                                };
                                let cas_new = State {
                                    phase: Phase::Waiting,
                                    notify_waiters_calls: curr.notify_waiters_calls,
                                };
                                let res = cell_cas(&notify.state, cas_curr, cas_new);

                                if let Err(actual) = res {
                                    assert_matches!(actual.phase, Phase::Notified);
                                    curr = actual;
                                } else {
                                    break;
                                }
                            }
                            Phase::Waiting => {
                                break;
                            }
                            Phase::Notified => {
                                let cas_curr = State {
                                    phase: Phase::Notified,
                                    notify_waiters_calls: curr.notify_waiters_calls,
                                };
                                let cas_new = State {
                                    phase: Phase::Empty,
                                    notify_waiters_calls: curr.notify_waiters_calls,
                                };
                                let res = cell_cas(&notify.state, cas_curr, cas_new);

                                match res {
                                    Ok(()) => {
                                        state.phase = Phase::Notified;
                                        continue 'outer_loop;
                                    }
                                    Err(actual) => {
                                        assert_matches!(actual.phase, Phase::Empty);
                                        curr = actual;
                                    }
                                }
                            }
                        }
                    }

                    let mut old_waker = None;
                    if waker.is_some() {
                        unsafe {
                            old_waker = mem::replace((&mut *waiter.waker.get()), waker);
                        }
                    }

                    waiters.push_front(NonNull::from(waiter));

                    state.phase = Phase::Waiting;

                    drop(waiters);
                    drop(old_waker);

                    return Poll::Pending;
                }
                Phase::Waiting => {
                    if waiter.notification.get().is_some() {
                        drop(unsafe { (*waiter.waker.get()).take() });

                        waiter.notification.set(None);
                        state.phase = Phase::Notified;
                        return Poll::Ready(());
                    }

                    let mut old_waker = None;
                    let mut waiters = notify.waiters.borrow_mut();

                    if waiter.notification.get().is_some() {
                        old_waker = unsafe { (*waiter.waker.get()).take() };

                        waiter.notification.set(None);

                        drop(waiters);
                        drop(old_waker);

                        state.phase = Phase::Notified;
                        return Poll::Ready(());
                    }

                    let curr = notify.state.get();

                    if curr.notify_waiters_calls != *notify_waiters_calls {
                        old_waker = unsafe { (*waiter.waker.get()).take() };

                        unsafe { waiters.remove(NonNull::from(waiter)) };

                        state.phase = Phase::Notified;
                    } else {
                        unsafe {
                            let v = &mut *waiter.waker.get();
                            if let Some(waker) = waker {
                                let should_update = match &*v {
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

                        return Poll::Pending;
                    }

                    drop(waiters);

                    drop(old_waker);
                }
                Phase::Notified => {
                    return Poll::Ready(());
                }
            }
        }
    }

    fn drop_notified(self) {
        let NotifiedProject {
            notify,
            state,
            waiter,
            ..
        } = self;

        if matches!(state.phase, Phase::Waiting) {
            let mut waiters = notify.waiters.borrow_mut();
            let mut notify_state = notify.state.get();

            let notification = waiter.notification.get();

            unsafe { waiters.remove(NonNull::from(waiter)) };

            if waiters.is_empty() && matches!(notify_state.phase, Phase::Waiting) {
                notify_state.phase = Phase::Empty;
                notify.state.set(notify_state);
            }

            if let Some(Notification::One(strategy)) = notification {
                if let Some(waker) =
                    notify_locked(&mut waiters, &notify.state, notify_state, strategy)
                {
                    drop(waiters);
                    waker.wake();
                }
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
