use crate::runtime::context;

#[track_caller]
pub fn rand32_next_u32_below(max: u32) -> u32 {
    context::with_handle(|handle| handle.rng().borrow_mut().next_u32_below(max))
}
