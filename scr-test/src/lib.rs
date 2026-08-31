pub mod task;

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let rt = scr::Runtime::new().unwrap();

    rt.block_on(future)
}
