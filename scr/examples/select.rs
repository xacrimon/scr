use std::time::Duration;

use scr::Runtime;
use scr::time;
use scr::select;
use std::time::Instant;

fn main() {
    Runtime::new().unwrap().block_on(alternate());
}

async fn alternate() {
    let period = Duration::from_millis(500);
    let mut int1 = time::interval(period);
    let mut int2 = time::interval_at(Instant::now()+period/2, period);

    loop {
        select! {
            _ = int1.tick() => println!("one"),
            _ = int2.tick() => println!("two"),
        }
    }
}
