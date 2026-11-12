use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;

/// Runs `work` on the main thread, on a later turn of its run loop.
///
/// This is how worker threads reach AppKit and WebKit, and how a callback
/// defers work that must not run inside itself.
pub fn on_main(work: impl FnOnce(MainThreadMarker) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        work(mtm);
    });
}
