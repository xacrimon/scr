mod sched;
mod context;
mod task;

use slab::Slab;
use task::Task;
use std::rc::Rc;

pub struct Runtime {
}

struct State {
    tasks: Slab<Task<()>>,
    queue: sched::Queue,
}