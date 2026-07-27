//! Covers docs/04_テスト仕様書.md §4.8 メモリリーク検出 (TC-LEAK-*).
//!
//! tests/fixtures/bp_target.c allocates `leak_ptr` (64 bytes, never freed)
//! and `ok_ptr` (32 bytes, freed immediately), specifically so the report
//! can be checked against a known outstanding/reclaimed split.

mod common;

use common::{build_fixture, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// TC-LEAK-01, TC-LEAK-02, TC-LEAK-03: tracking starts, the freed
/// allocation is not reported, and the leaked one is.
#[test]
fn leak_report_finds_only_the_unfree_allocation() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("leak start");
    let out = session.wait_for("of 3 functions hooked", TIMEOUT);
    assert!(out.contains("Leak tracking started"), "unexpected leak-start message:\n{out}");

    // Report *while the process is still alive*, right after both malloc
    // calls and the one free (bp_target.c:44) but before it exits: exiting
    // clears all tracking state (there's no allocation left to report from
    // a process that no longer exists), so `leak report` only makes sense
    // before that point.
    session.send("break bp_target.c:46");
    session.wait_for("Breakpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Breakpoint hit at", TIMEOUT);

    session.send("leak report");
    // Wait for the tail of the summary line: it's printed after "1
    // outstanding allocation(s)," so its presence guarantees that part has
    // already arrived too.
    let out = session.wait_for("64 bytes total:", TIMEOUT);
    assert!(
        out.contains("1 outstanding allocation(s), 64 bytes total:"),
        "expected exactly the 64-byte leak to be reported:\n{out}"
    );

    session.send_all(&["continue", "quit"]);
    session.finish(TIMEOUT);
}

/// TC-LEAK-04: `leak stop` disables tracking.
#[test]
fn leak_stop_disables_tracking() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("leak start");
    session.wait_for("Leak tracking started", TIMEOUT);
    session.send("leak stop");
    session.wait_for("Leak tracking stopped.", TIMEOUT);
    session.finish(TIMEOUT);
}

/// TC-LEAK-05: starting twice in a row is rejected.
#[test]
fn double_start_is_rejected() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "leak start", "leak start", "quit"]);
    // The rejection message goes to stderr, which can arrive in more than
    // one chunk and interleave with the next stdout prompt when polled
    // mid-session; check the fully-drained output instead.
    let out = session.finish(TIMEOUT);
    assert!(out.contains("Leak tracking started"), "unexpected leak-start message:\n{out}");
    assert!(
        out.contains("Leak tracking is already running."),
        "expected the second `leak start` to be rejected:\n{out}"
    );
}

/// TC-LEAK-06: reporting before tracking has started is rejected.
#[test]
fn report_without_start_is_rejected() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "leak report", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(
        out.contains("Leak tracking is not running. Use 'leak start' first."),
        "expected the not-tracking message:\n{out}"
    );
}
