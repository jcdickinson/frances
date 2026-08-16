use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use frances_worker::{SearchOutcome, find_or_grep};
use frances_worker_protocol::{Feed, FileSearchEvent, FileSearchOptions, FileSearchQuery};

#[test]
fn cancellation_stops_the_walk() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(directory.path().join(format!("{index}.txt")), "content").unwrap();
    }
    let checks = AtomicUsize::new(0);
    let result = find_or_grep(
        FileSearchOptions {
            cwd: Some(directory.path().to_path_buf()),
            root: None,
            query: FileSearchQuery::All,
            exclude: Vec::new(),
            ignore: true,
            hidden: false,
            depth: None,
        },
        || checks.fetch_add(1, Ordering::Relaxed) >= 5,
        |_| true,
    )
    .unwrap();

    assert_eq!(result, SearchOutcome::Cancelled);
    assert!(checks.load(Ordering::Relaxed) < 100);
}

#[test]
fn dropping_a_backpressured_feed_stops_the_walk() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(directory.path().join(format!("{index}.txt")), "content").unwrap();
    }
    let options = FileSearchOptions {
        cwd: Some(directory.path().to_path_buf()),
        root: None,
        query: FileSearchQuery::All,
        exclude: Vec::new(),
        ignore: true,
        hidden: false,
        depth: None,
    };
    let (sender, results) = Feed::<FileSearchEvent>::channel();
    let emitted = Arc::new(AtomicUsize::new(0));
    let thread_emitted = emitted.clone();
    let search = std::thread::spawn(move || {
        let cancellation = sender.clone();
        find_or_grep(
            options,
            move || cancellation.is_closed(),
            move |event| {
                thread_emitted.fetch_add(1, Ordering::Relaxed);
                sender.blocking_send(event).is_ok()
            },
        )
    });

    while emitted.load(Ordering::Relaxed) < 17 {
        std::thread::yield_now();
    }
    drop(results);

    assert_eq!(search.join().unwrap().unwrap(), SearchOutcome::Cancelled);
}
