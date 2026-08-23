mod raw;
mod core;
mod waker;
mod state;
mod error;

use raw::RawTask;
use std::{marker::PhantomData, num::NonZeroU32};
use std::fmt;
use error::JoinError;

pub(crate) struct Task<T> {
    raw: RawTask,
    _marker: PhantomData<T>,
}

pub(crate) type Result<T> = std::result::Result<T, JoinError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id {
    v: NonZeroU32,
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.v.fmt(f)
    }
}