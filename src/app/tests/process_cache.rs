//! Process-cache and /proc CPU-times tests.
use super::{
    io_wait_percent, parse_cpu_times, per_cpu_usage_by_id, refresh_process_metadata, ProcessCache,
};

use crate::domain::ProcessIdentity;

#[test]
fn per_cpu_usage_is_indexed_by_kernel_cpu_id_with_holes() {
    // Online set 0,2-3 (cpu1 offline): usage lands at its ids, hole stays 0.
    let usage = per_cpu_usage_by_id(&[("cpu0", 10.0), ("cpu2", 30.0), ("cpu3", 40.0)]);
    assert_eq!(usage, vec![10.0, 0.0, 30.0, 40.0]);

    // Non-prefix start (cpuset/isolcpus): ids preserved, holes zeroed.
    let usage = per_cpu_usage_by_id(&[("cpu4", 4.0), ("cpu5", 5.0)]);
    assert_eq!(usage, vec![0.0, 0.0, 0.0, 0.0, 4.0, 5.0]);

    // Unparseable names fall back to compact order.
    let usage = per_cpu_usage_by_id(&[("weird", 1.0), ("other", 2.0)]);
    assert_eq!(usage, vec![1.0, 2.0]);
}

#[test]
fn cache_reuses_stable_metadata_for_same_identity() {
    let mut cache = ProcessCache::default();
    let identity = ProcessIdentity::new(0, 1234);
    let first = cache.stable(identity, "app", "/bin/app");
    let second = cache.stable(identity, "app", "/bin/app");
    assert_eq!(first, second);
    assert_eq!(cache.entries.len(), 1);
}

#[test]
fn cache_refreshes_static_metadata_when_it_changes_for_the_same_identity() {
    // `execve` keeps the PID and start time but changes the image.
    let mut cache = ProcessCache::default();
    let identity = ProcessIdentity::new(4242, 1000);
    assert_eq!(
        cache.stable(identity, "sh", "/bin/sh"),
        ("sh".to_string(), "/bin/sh".to_string())
    );
    assert_eq!(
        cache.stable(identity, "llama-server", "/usr/bin/llama-server"),
        (
            "llama-server".to_string(),
            "/usr/bin/llama-server".to_string()
        )
    );
    // An empty transient read keeps the last good value.
    assert_eq!(
        cache.stable(identity, "", ""),
        (
            "llama-server".to_string(),
            "/usr/bin/llama-server".to_string()
        )
    );
    assert_eq!(cache.entries.len(), 1);
}

#[test]
fn cache_treats_reused_pid_with_new_start_time_as_new_process() {
    let mut cache = ProcessCache::default();
    let old = ProcessIdentity::new(4242, 1000);
    let new = ProcessIdentity::new(4242, 2000);
    cache.stable(old, "old-prog", "/bin/old");
    let (_, command) = cache.stable(new, "new-prog", "/bin/new");
    assert_eq!(command, "/bin/new");
    assert_eq!(cache.entries.len(), 2);
}

#[test]
fn thread_count_is_resampled_each_refresh_and_recovers_after_a_failed_read() {
    let mut cache = ProcessCache::default();
    let identity = ProcessIdentity::new(4242, 1000);

    // First sighting: static metadata is cached, threads read fresh.
    let (program, command, first) =
        refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(4));
    assert_eq!((program.as_str(), command.as_str()), ("app", "/bin/app"));
    assert_eq!(first, Some(4));

    // Same identity: the static metadata is reused, but the thread count
    // is read again — a changed count reaches the next refresh.
    let (program, command, second) =
        refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(9));
    assert_eq!((program.as_str(), command.as_str()), ("app", "/bin/app"));
    assert_eq!(second, Some(9));

    // A transient read failure is unknown for that cycle, not a
    // permanent substitute...
    let (_, _, failed) =
        refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| None);
    assert_eq!(failed, None);

    // ...and the next successful read recovers.
    let (_, _, recovered) =
        refresh_process_metadata(&mut cache, identity, "app", "/bin/app", |_| Some(11));
    assert_eq!(recovered, Some(11));

    assert_eq!(cache.entries.len(), 1, "static metadata stays cached once");
}

#[test]
fn reused_pid_does_not_inherit_cached_static_metadata() {
    let mut cache = ProcessCache::default();
    let old = ProcessIdentity::new(5150, 1000);
    let new = ProcessIdentity::new(5150, 2000);
    refresh_process_metadata(
        &mut cache,
        old,
        "llama-server",
        "/usr/bin/llama-server",
        |_| Some(8),
    );
    let (program, command, threads) =
        refresh_process_metadata(&mut cache, new, "other", "/usr/bin/other", |_| Some(2));
    assert_eq!(
        (program.as_str(), command.as_str()),
        ("other", "/usr/bin/other")
    );
    assert_eq!(threads, Some(2));
    assert_eq!(cache.entries.len(), 2);
}

#[test]
fn cache_retain_drops_exited_processes() {
    let mut cache = ProcessCache::default();
    let alive = ProcessIdentity::new(0, 1);
    let gone = ProcessIdentity::new(0, 2);
    cache.stable(alive, "a", "a");
    cache.stable(gone, "b", "b");
    cache.retain(std::iter::once(alive));
    assert_eq!(cache.entries.len(), 1);
}

#[test]
fn parses_linux_cpu_times_and_iowait_delta() {
    let previous = parse_cpu_times(
        "cpu  100 0 50 800 20 0 10 0 0 0
",
    )
    .unwrap();
    let current = parse_cpu_times(
        "cpu  120 0 60 850 25 0 15 0 0 0
",
    )
    .unwrap();
    assert_eq!(previous.io_wait, 20);
    assert_eq!(current.io_wait, 25);
    let io_wait = io_wait_percent(previous, current).unwrap();
    assert!((io_wait - 5.555555556).abs() < 0.001);
}
