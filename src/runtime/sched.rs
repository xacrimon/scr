use std::collections::VecDeque;
use crate::runtime::task;
use std::marker::PhantomData;
use std::cell::UnsafeCell;

const BASE_QUEUE_SIZE: usize = 32;

pub(crate) struct Queue {
    inner: UnsafeCell<VecDeque<task::Id>>,
    _marker: PhantomData<*const ()>,
}

impl Queue {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(VecDeque::with_capacity(BASE_QUEUE_SIZE)),
            _marker: PhantomData,
        }
    }

    pub(crate) fn push(&self, runnable: task::Id) {
        unsafe {
            (*self.inner.get()).push_back(runnable);
        }
    }

    pub(crate) fn pop(&self) -> Option<task::Id> {
        unsafe { (*self.inner.get()).pop_front() }
    }

    pub(crate) fn len(&self) -> usize {
        unsafe { (*self.inner.get()).len() }
    }
}