use std::time::Duration;

use galdeck_core::wake_channel;

#[test]
fn a_wake_up_is_delivered() {
    let (waker, rx) = wake_channel();
    waker.notify();
    assert!(rx.wait(Duration::from_millis(50)));
}

#[test]
fn notify_never_blocks_however_many_times_it_is_called() {
    // One pending wake-up is exactly as informative as a hundred, because the
    // loop drains every inbox when it wakes. If this ever blocked, a busy
    // core would stall its own producers.
    let (waker, rx) = wake_channel();
    for _ in 0..10_000 {
        waker.notify();
    }
    rx.drain();
    // The slot is empty again, so a wait now times out rather than returning
    // a backlog of stale tokens.
    let start = std::time::Instant::now();
    assert!(rx.wait(Duration::from_millis(20)));
    assert!(start.elapsed() >= Duration::from_millis(15));
}

#[test]
fn dropping_every_waker_ends_the_wait() {
    let (waker, rx) = wake_channel();
    drop(waker);
    assert!(
        !rx.wait(Duration::from_millis(50)),
        "a disconnected waker is the loop's signal to exit"
    );
}
