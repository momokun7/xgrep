// rust/tests/watch_threads_test.rs
//
// Starting and stopping the file watcher must not leave threads behind.
// Kept as the only test in this binary so other tests' threads do not
// disturb the count.
use std::fs;
use std::time::{Duration, Instant};
use xgrep_search::{Config, SearchOptions, WatchStatus, Xgrep};

#[cfg(target_os = "linux")]
fn thread_count() -> usize {
    fs::read_dir("/proc/self/task").unwrap().count()
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> usize {
    0
}

#[test]
fn test_watcher_start_stop_does_not_leak_threads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn alpha() {}\n").unwrap();

    // Warm up: index build and a search start rayon's global pool, which
    // lives for the whole process.
    {
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        xg.search("alpha", &SearchOptions::new()).unwrap();
    }
    let baseline = thread_count();

    for i in 0..5 {
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.start_watching().unwrap();
        assert!(
            xg.wait_for_watcher(Duration::from_secs(30)),
            "{:?}",
            xg.watch_status()
        );
        fs::write(
            root.join(format!("n{i}.rs")),
            format!("fn new_{i}() {{}}\n"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let r = xg
                .search(&format!("new_{i}"), &SearchOptions::new())
                .unwrap();
            if r.len() == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "edit not seen");
            std::thread::sleep(Duration::from_millis(20));
        }
        if i % 2 == 0 {
            xg.stop_watching();
            assert_eq!(xg.watch_status(), WatchStatus::Off);
        }
        // Odd iterations: dropping the Xgrep stops the watcher.
    }

    // The OS watcher's own event thread exits asynchronously after its
    // handle is dropped; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    while thread_count() > baseline && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        thread_count() <= baseline,
        "threads: baseline {baseline}, now {}",
        thread_count()
    );
}
