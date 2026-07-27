//! Covers docs/04_テスト仕様書.md §4.5 スレッド管理 (TC-TH-*).

mod common;

use common::{build_fixture, pick_non_main_thread_id, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// TC-TH-01: while worker threads are alive, `threads` lists all of them
/// (main included) tagged with their pid.
#[test]
fn threads_lists_all_live_threads() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    // A watchpoint hit guarantees at least one worker thread is alive and
    // the process is stopped, so `threads` has more than just main to show.
    session.send("watch g_shared_counter");
    session.wait_for("Watchpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Watchpoint #1 hit", TIMEOUT);

    // Wait for the *next* prompt after sending `threads`, not just "(main)":
    // `threads` prints one line per thread and thread order isn't fixed
    // relative to which one happens to be tagged (main), so only the prompt
    // reappearing guarantees every line has fully arrived.
    let prompts_before = session.count("(tdb) ");
    session.send("threads");
    let out = session.wait_for_count("(tdb) ", prompts_before + 1, TIMEOUT);
    let thread_lines: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("(pid "))
        .collect();
    assert!(
        thread_lines.len() >= 2,
        "expected the main thread plus at least one worker:\n{out}"
    );
    session.finish(Duration::from_secs(20));
}

/// TC-TH-03: switching the current thread updates subsequent register
/// output to that thread's own context.
#[test]
fn thread_switch_changes_current_context() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("watch g_shared_counter");
    session.wait_for("Watchpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Watchpoint #1 hit", TIMEOUT);

    let prompts_before = session.count("(tdb) ");
    session.send("threads");
    let out = session.wait_for_count("(tdb) ", prompts_before + 1, TIMEOUT);
    let other_tid = pick_non_main_thread_id(&out);

    session.send(&format!("thread {other_tid}"));
    let out = session.wait_for(&format!("Switched to thread {other_tid}"), TIMEOUT);
    let _ = out;
    session.finish(Duration::from_secs(20));
}

/// TC-TH-04: switching to an id that doesn't exist is an error, not a crash.
#[test]
fn switching_to_unknown_thread_is_an_error() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "thread 999999", "quit"]);
    // This message goes to stderr, which (unlike stdout) isn't line-
    // buffered on the writing side, so it can arrive in more than one
    // chunk and interleave with the next stdout prompt when read
    // concurrently. Check the fully-drained output instead of racing a
    // `wait_for` against it.
    let out = session.finish(TIMEOUT);
    assert!(out.contains("No such thread:"), "expected an error message:\n{out}");
    assert!(out.contains("999999"), "expected the unknown id to be echoed back:\n{out}");
}

/// TC-TH-05, TC-TH-06: `lock` stops every other thread from making
/// progress until `unlock` releases them.
#[test]
fn lock_and_unlock_round_trip() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("lock");
    let out = session.wait_for("all other threads frozen until 'unlock'.", TIMEOUT);
    assert!(out.contains("Locked to thread"), "unexpected lock message:\n{out}");

    session.send("unlock");
    let out = session.wait_for("all threads resumed.", TIMEOUT);
    assert!(out.contains("Unlocked thread"), "unexpected unlock message:\n{out}");

    session.send_all(&["continue", "quit"]);
    session.finish(Duration::from_secs(20));
}
