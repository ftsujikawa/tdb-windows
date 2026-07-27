//! Covers docs/04_テスト仕様書.md §4.3 ウォッチポイント (TC-WP-*).

mod common;

use common::{build_fixture, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// TC-WP-01, TC-WP-02: a write watchpoint on a global fires regardless of
/// which worker thread performs the write, reporting old/new values.
#[test]
fn write_watchpoint_hits_across_threads() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("watch g_shared_counter");
    session.wait_for("Watchpoint #1 set on 'g_shared_counter'", TIMEOUT);

    // Wait for "New value =", the *last* of the three lines a hit prints
    // (hit banner, then old value, then new value): that guarantees the
    // earlier two lines are already fully in the buffer too.
    session.send("continue");
    let out = session.wait_for("New value = 1", TIMEOUT);
    assert!(out.contains("Watchpoint #1 hit: g_shared_counter"), "missing hit banner:\n{out}");
    assert!(out.contains("Old value = 0"), "missing old value:\n{out}");

    session.send("continue");
    session.wait_for("New value = 2", TIMEOUT);
    session.finish(Duration::from_secs(20));
}

/// TC-WP-03: an access (read/write) watchpoint is announced as such and
/// still fires.
#[test]
fn access_watchpoint_fires() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("awatch g_shared_counter");
    let out = session.wait_for("read/write", TIMEOUT);
    assert!(out.contains("Watchpoint #1 set on 'g_shared_counter'"), "unexpected set message:\n{out}");

    session.send("continue");
    session.wait_for("Watchpoint #1 hit", TIMEOUT);
    session.finish(Duration::from_secs(20));
}

/// TC-WP-04: only 4 hardware watchpoint slots exist; a 5th request fails
/// cleanly.
#[test]
fn fifth_watchpoint_is_rejected() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send_all(&[
        "watch g_shared_counter",
        "watch g_shared_counter",
        "watch g_shared_counter",
        "watch g_shared_counter",
        "watch g_shared_counter",
        "quit",
    ]);
    let out = session.finish(TIMEOUT);

    let set_count = out.matches("Watchpoint #").count();
    assert_eq!(set_count, 4, "expected exactly 4 watchpoints to be accepted:\n{out}");
    assert!(
        out.contains("Cannot set watchpoint: all 4 hardware watchpoint slots are already in use."),
        "expected the 5th watchpoint to be rejected:\n{out}"
    );
}

/// TC-WP-06, TC-WP-07: listing and deleting a watchpoint.
#[test]
fn list_and_delete_watchpoint() {
    let exe = build_fixture("threads_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("watch g_shared_counter");
    session.wait_for("Watchpoint #1 set", TIMEOUT);

    // The `watch` confirmation above already printed "g_shared_counter"
    // once; wait for the *second* occurrence (from the listing itself).
    session.send("watchpoints");
    let out = session.wait_for_count("g_shared_counter", 2, TIMEOUT);
    assert!(out.contains("#1 0x"), "watchpoint listing missing entry:\n{out}");

    session.send("deletewatch 1");
    session.wait_for("Deleted watchpoint #1", TIMEOUT);
    session.send("watchpoints");
    let out = session.wait_for("No watchpoints.", TIMEOUT);
    let _ = out;
    session.finish(Duration::from_secs(20));
}
