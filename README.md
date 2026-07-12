# tdb-windows

A tiny command-line debugger for C programs on Windows, written in Rust.

## Features

- Launch a Windows process with `DEBUG_ONLY_THIS_PROCESS`
- Wait for and dispatch debug events
- Set/remove software breakpoints (INT3)
- Single step
- Read target memory and registers (x64)
- Symbol loading via DbgHelp (`SymInitializeW`, `SymFromAddrW`, etc.)
- Interactive REPL with basic commands

## Build

Requires Rust and the MSVC toolchain on Windows.

```powershell
cargo build --release
```

## Usage

```powershell
.\target\release\tdb-windows.exe <target.exe> [args...]
```

Example:

```powershell
.\target\release\tdb-windows.exe .\test.exe
```

## Commands

```
run                          Start or restart the target
continue                     Continue execution
step                         Step into
next                         Step over
break <addr|symbol>          Set breakpoint
delete <addr>                Delete breakpoint
breakpoints                  List breakpoints
registers                    Show x64 registers
memory <addr> [count]        Dump memory
backtrace                    Show call stack (placeholder)
help                         Show this help
quit                         Quit
```

## Notes

- This is a base/debugger skeleton, not a production debugger.
- Only x64 targets are assumed in the register view.
- Make sure debug symbols (PDB) are available for symbol resolution.
