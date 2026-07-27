//! Covers docs/04_テスト仕様書.md §4.4 実行制御(ステップ実行) (TC-STEP-*).

mod common;

use common::{build_fixture, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// TC-STEP-01: a single `step` from the entry breakpoint moves forward and
/// reports a new stop location.
#[test]
fn step_into_reports_a_new_stop() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "break main", "continue", "step", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(out.contains("Step at"), "expected a step-stop message:\n{out}");
}

/// TC-STEP-02, TC-STEP-03: `next` steps over a CALL instruction (here,
/// `add(5, 3)` on bp_target.c:32) instead of descending into it, and a
/// single `next` carries a whole multi-statement source line's worth of
/// call-skipping automatically.
#[test]
fn step_over_does_not_enter_the_called_function() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "break main", "continue"]);
    // bp_target.c lines 21,22,23,24,25,27,29,30,32 are the 9 statements from
    // `int x = 10;` through the `add(5, 3)` call (inclusive); one `next`
    // per statement walks up to and over that call without ever entering
    // `add`'s body.
    for _ in 0..9 {
        session.send("next");
    }
    session.send_all(&["continue", "quit"]);
    let out = session.finish(TIMEOUT);

    assert!(
        !out.contains("[add"),
        "`next` should never single-step into add()'s body:\n{out}"
    );
    assert!(out.contains("8"), "add(5, 3) should still have executed (and printed 8):\n{out}");
    let pid = common::root_pid(&out);
    assert!(
        out.contains(&format!("Process {pid} exited with code 0")),
        "target should run to completion after stepping:\n{out}"
    );
}

/// TC-STEP-04: `finish` runs until the current function returns to its
/// caller.
#[test]
fn finish_returns_to_caller() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("break add");
    session.wait_for("Breakpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Breakpoint hit at", TIMEOUT);
    session.send("finish");
    let out = session.wait_for("[main", TIMEOUT);
    assert!(out.contains("Returned to"), "finish should have reported a return:\n{out}");

    session.send_all(&["continue", "quit"]);
    session.finish(TIMEOUT);
}
