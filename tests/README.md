# tdb-windows integration tests

This suite automates a representative subset of `docs/04_テスト仕様書.md`
against the real `tdb-windows.exe`: each test spawns it against a small C
fixture (`tests/fixtures/*.c`), drives it through stdin like a REPL user
would, and asserts on the text it prints to stdout/stderr.

## Running

```powershell
cargo test
```

The first run compiles the C fixtures with MSVC (`cl.exe`), either directly
(if already in a Developer Command Prompt) or by locating Visual Studio via
`vswhere.exe` and invoking `cl.exe` through `vcvars64.bat`. Fixture `.exe`s
are cached in `tests/fixtures/` and only rebuilt when their `.c` source
changes.

## Layout

| File | Test spec section |
|---|---|
| `common/mod.rs` | Shared harness (`TdbSession`, fixture building) — not a test itself. |
| `process_control.rs` | §4.1 プロセス制御 (TC-PROC-*) |
| `breakpoints.rs` | §4.2 ブレークポイント (TC-BP-*) |
| `watchpoints.rs` | §4.3 ウォッチポイント (TC-WP-*) |
| `stepping.rs` | §4.4 実行制御(ステップ実行) (TC-STEP-*) |
| `threads.rs` | §4.5 スレッド管理 (TC-TH-*) |
| `memory_registers.rs` | §4.6 レジスタ・メモリ・逆アセンブル (TC-MEM-*) |
| `expressions.rs` | §4.7 式評価・変数表示 (TC-EXPR-*) |
| `leak_detection.rs` | §4.8 メモリリーク検出 (TC-LEAK-*) |
| `repl_errors.rs` | §4.9 REPL・ヘルプ・エラー処理 (TC-REPL-*) |

Fixtures (`tests/fixtures/*.c`):

- `bp_target.c` — breakpoint/step/backtrace/memory/expression/leak tests. Line
  numbers are load-bearing (several tests reference `bp_target.c:<line>`
  directly); only append new statements, don't renumber existing ones.
- `threads_target.c` — three worker threads incrementing a shared global
  under a lock, for watchpoint and thread-management tests.
- `child_target.c` — spawns a child (`cmd.exe`) which spawns its own child
  (`ping.exe`), for the root/child/grandchild process-tracking tests.

## Not automated here

A few spec items are inherently unsuited to an automated, single-machine
test run, and are left as manual checks (see the spec document itself):

- **TC-PROC-11** (Ctrl+C forwarding) — requires sending a real console
  control event to the test process, which behaves differently under a
  test harness than a real console.
- **TC-TH-08** (a thread that denies the debugger `GetContext`/`SetContext`)
  — depends on OS/process-specific protected threads that aren't reliably
  reproducible on demand.
- **TC-PRIV-01 / TC-PRIV-02** (`SeDebugPrivilege` enabled vs. not) — depends
  on whether the test run itself is elevated, which isn't something a test
  should assume or toggle.
- **TC-PROC-12** (`quit` leaves no orphaned processes) — the harness's
  `Drop` impl already kills the session if a test fails partway, so this is
  exercised indirectly by every test, but isn't asserted on directly (it
  would require enumerating system processes by name, which is fragile in
  a shared CI environment).

Everything else in `04_テスト仕様書.md` has at least one corresponding
`#[test]`; see the doc comment atop each corresponding TC-ID's test for the
exact mapping.
