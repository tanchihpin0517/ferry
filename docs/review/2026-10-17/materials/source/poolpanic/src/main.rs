use ferry::pool::{self, Profiled};
use ferry::profile::State;
use std::time::Duration;

fn main() {
    // Watchdog: if the pool has not finished in 5 s, it hangs.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(5));
        eprintln!("HANG: the pool did not finish within 5 s after a worker panicked");
        std::process::exit(2);
    });
    let profiled = Profiled { role: "test", work: State::Run, full: State::Work };
    std::thread::scope(|scope| {
        let (mut submitter, collector) = pool::ordered(scope, 8, 64, profiled, |item: u64| {
            if item == 3 {
                panic!("worker panics on item 3");
            }
            item
        });
        scope.spawn(move || {
            for item in 0..1000 {
                if submitter.submit(item).is_err() {
                    return;
                }
            }
        });
        let taken = collector.count();
        eprintln!("collector ended after {taken} results");
    });
}
