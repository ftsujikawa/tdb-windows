//! Covers docs/04_テスト仕様書.md §4.6 レジスタ・メモリ・逆アセンブル (TC-MEM-*).

mod common;

use common::{build_fixture, strip_prompt, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn register_value(registers_output: &str, name: &str) -> String {
    registers_output
        .lines()
        .find_map(|l| strip_prompt(l).strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("{name} line missing from `registers` output:\n{registers_output}"))
        .trim()
        .to_string()
}

/// TC-MEM-01: `registers` shows the full x64 register set.
#[test]
fn registers_shows_full_context() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "registers", "quit"]);
    let out = session.finish(TIMEOUT);

    for expected in ["RIP:", "RSP:", "RAX:", "EFLAGS:", "MXCSR:", "XMM0"] {
        assert!(out.contains(expected), "missing {expected} in registers output:\n{out}");
    }
}

/// TC-MEM-02: `memory`/`x` dumps bytes from an explicit address in the
/// standard "address: hex bytes |ascii|" layout.
#[test]
fn memory_dump_at_stack_pointer() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("registers");
    // Wait for the line *after* RSP's, so RSP's own line (value + newline)
    // is guaranteed to be fully captured before we try to parse it.
    let out = session.wait_for("RBP:", TIMEOUT);
    let rsp = register_value(&out, "RSP");

    // Check for "<address>: ", the dump line's own prefix format, not just
    // the address string alone: that value is already present verbatim in
    // the earlier `registers` output ("RSP: <address>"), so a plain
    // `contains(&rsp)` would pass even if the dump itself printed nothing.
    session.send(&format!("x {rsp} 32"));
    let out = session.wait_for("|", TIMEOUT);
    assert!(
        out.contains(&format!("{rsp}: ")),
        "expected the dump to start at {rsp}:\n{out}"
    );
    session.finish(TIMEOUT);
}

/// TC-MEM-03, TC-MEM-04: disassembly at the current position and at an
/// explicit symbol both produce instruction listings.
#[test]
fn disassemble_current_and_by_symbol() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "disassemble", "disassemble main 5", "quit"]);
    let out = session.finish(TIMEOUT);

    let disasm_lines = out
        .lines()
        .filter(|l| strip_prompt(l.trim_start()).starts_with("0x") && l.contains(':'))
        .count();
    assert!(disasm_lines >= 2, "expected at least a couple of disassembled instructions:\n{out}");
}

/// TC-MEM-05: `backtrace` reports the call chain, deepest frame first.
#[test]
fn backtrace_shows_call_chain() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("break add");
    session.wait_for("Breakpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Breakpoint hit at", TIMEOUT);
    // Wait for frame #1's marker ("[main"), which is printed after frame
    // #0's whole line: that guarantees #0's line ("#0 ... [add...]") has
    // already fully arrived too.
    session.send("backtrace");
    let out = session.wait_for("[main", TIMEOUT);

    assert!(out.contains("#0") && out.contains("[add"), "frame #0 should be inside add:\n{out}");
    assert!(out.contains("#1"), "frame #1 should be present:\n{out}");
    session.finish(TIMEOUT);
}

/// TC-MEM-06: `list` shows the source around the current position with the
/// current line marked.
#[test]
fn list_shows_current_source_line() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "break main", "continue", "list", "quit"]);
    let out = session.finish(TIMEOUT);

    assert!(
        out.contains("=>") && out.contains("int x = 10;"),
        "expected the current line to be marked in the source listing:\n{out}"
    );
}
