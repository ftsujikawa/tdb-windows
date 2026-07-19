use crate::error::{DebuggerError, Result};
use crate::watchpoint::WatchKind;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShowKind {
    Locals,
    Args,
    Globals,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeakAction {
    Start,
    Stop,
    Report,
}

// A `usize` limit where `None` means "unlimited" (gdb's convention for
// `set print elements`/`set print repeats`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrintOption {
    Pretty(bool),
    Address(bool),
    Elements(Option<usize>),
    Repeats(Option<usize>),
}

#[derive(Debug, Clone)]
pub enum Command {
    Run,
    Continue,
    StepInto,
    StepOver,
    Up,
    Break(String),
    Delete(usize),
    ListBreakpoints,
    Watch(String, WatchKind),
    DeleteWatch(usize),
    ListWatchpoints,
    Threads,
    Thread(u32),
    Lock(Option<u32>),
    Unlock,
    Registers,
    Memory { address: String, count: usize },
    Backtrace,
    List(Option<String>),
    Print(String, Option<char>),
    Set(String),
    Show(ShowKind),
    Leak(LeakAction),
    SetPrint(PrintOption),
    ShowPrintSettings,
    Help,
    Quit,
    Empty,
}

pub fn parse(input: &str) -> Result<Command> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Command::Empty);
    }

    let mut parts = trimmed.split_whitespace();
    let cmd_token = parts.next().unwrap_or("").to_lowercase();
    let args: Vec<&str> = parts.collect();

    // GDB-style "p/x", "print/s expr": a format letter suffixed onto the
    // command word after a '/', not a separate argument.
    let (cmd, fmt) = match cmd_token.split_once('/') {
        Some((base, spec)) => (base.to_string(), Some(spec.to_string())),
        None => (cmd_token, None),
    };

    match cmd.as_str() {
        "r" | "run" => Ok(Command::Run),
        "c" | "cont" | "continue" => Ok(Command::Continue),
        "s" | "step" | "si" => Ok(Command::StepInto),
        "n" | "next" | "so" => Ok(Command::StepOver),
        "up" | "finish" => Ok(Command::Up),
        "b" | "break" | "bp" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "break requires an address or symbol".to_string(),
                ));
            }
            Ok(Command::Break(args.join(" ")))
        }
        "d" | "delete" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "delete requires a breakpoint number".to_string(),
                ));
            }
            let id = args[0].parse::<usize>().map_err(|_| {
                DebuggerError::InvalidCommand(format!(
                    "invalid breakpoint number: {}",
                    args[0]
                ))
            })?;
            Ok(Command::Delete(id))
        }
        "bl" | "breakpoints" => Ok(Command::ListBreakpoints),
        "watch" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "watch requires an expression".to_string(),
                ));
            }
            Ok(Command::Watch(args.join(" "), WatchKind::Write))
        }
        "awatch" | "rwatch" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "awatch requires an expression".to_string(),
                ));
            }
            Ok(Command::Watch(args.join(" "), WatchKind::Access))
        }
        "dw" | "deletewatch" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "deletewatch requires a watchpoint number".to_string(),
                ));
            }
            let id = args[0].parse::<usize>().map_err(|_| {
                DebuggerError::InvalidCommand(format!(
                    "invalid watchpoint number: {}",
                    args[0]
                ))
            })?;
            Ok(Command::DeleteWatch(id))
        }
        "wl" | "watchpoints" => Ok(Command::ListWatchpoints),
        "threads" => Ok(Command::Threads),
        "thread" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "thread requires a thread id".to_string(),
                ));
            }
            let id = args[0].parse::<u32>().map_err(|_| {
                DebuggerError::InvalidCommand(format!("invalid thread id: {}", args[0]))
            })?;
            Ok(Command::Thread(id))
        }
        "lock" => {
            if args.is_empty() {
                return Ok(Command::Lock(None));
            }
            let id = args[0].parse::<u32>().map_err(|_| {
                DebuggerError::InvalidCommand(format!("invalid thread id: {}", args[0]))
            })?;
            Ok(Command::Lock(Some(id)))
        }
        "unlock" => Ok(Command::Unlock),
        "l" | "list" => Ok(Command::List(if args.is_empty() {
            None
        } else {
            Some(args.join(" "))
        })),
        "regs" | "registers" | "info" => Ok(Command::Registers),
        "x" | "mem" | "memory" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "memory requires an address".to_string(),
                ));
            }
            let address = args[0].to_string();
            let count = args.get(1).unwrap_or(&"32").parse().unwrap_or(32);
            Ok(Command::Memory { address, count })
        }
        "bt" | "tb" | "backtrace" => Ok(Command::Backtrace),
        "p" | "print" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "print requires an expression".to_string(),
                ));
            }
            // GDB accepts the format both attached ("p/x foo") and as its
            // own space-separated token ("p /x foo"); only fall back to the
            // latter when the command word itself had no "/fmt" suffix, so
            // "p/x" doesn't also look for a second format in args[0].
            let (fmt, expr_args): (Option<String>, &[&str]) = if fmt.is_none() {
                match args[0].strip_prefix('/') {
                    Some(spec) => (Some(spec.to_string()), &args[1..]),
                    None => (fmt, &args[..]),
                }
            } else {
                (fmt, &args[..])
            };
            if expr_args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "print requires an expression".to_string(),
                ));
            }
            let fmt = parse_print_format(fmt.as_deref())?;
            Ok(Command::Print(expr_args.join(" "), fmt))
        }
        "set" => {
            if args.is_empty() {
                return Err(DebuggerError::InvalidCommand(
                    "set requires an assignment, e.g. set i = 5".to_string(),
                ));
            }
            if args[0].eq_ignore_ascii_case("print") {
                return parse_set_print(&args[1..]).map(Command::SetPrint);
            }
            Ok(Command::Set(args.join(" ")))
        }
        "show" => {
            if args.first().map(|s| s.eq_ignore_ascii_case("print")) == Some(true) {
                return Ok(Command::ShowPrintSettings);
            }
            let kind = match args.first().map(|s| s.to_lowercase()).as_deref() {
                Some("locals") => ShowKind::Locals,
                Some("args") => ShowKind::Args,
                Some("globals") => ShowKind::Globals,
                _ => {
                    return Err(DebuggerError::InvalidCommand(
                        "show requires: locals, args, globals, or print".to_string(),
                    ))
                }
            };
            Ok(Command::Show(kind))
        }
        "leak" | "leaks" => {
            let action = match args.first().map(|s| s.to_lowercase()).as_deref() {
                Some("start") | Some("on") => LeakAction::Start,
                Some("stop") | Some("off") => LeakAction::Stop,
                Some("report") | Some("show") | None => LeakAction::Report,
                Some(other) => {
                    return Err(DebuggerError::InvalidCommand(format!(
                        "unknown leak action: {} (expected start, stop, or report)",
                        other
                    )))
                }
            };
            Ok(Command::Leak(action))
        }
        "h" | "help" | "?" => Ok(Command::Help),
        "q" | "quit" | "exit" => Ok(Command::Quit),
        _ => Err(DebuggerError::InvalidCommand(format!("unknown command: {}", cmd))),
    }
}

// GDB print format letters this debugger understands: x (hex), d (signed
// decimal), u (unsigned decimal), o (octal), t (binary), c (char), f
// (float), a (address, symbol-annotated), z (zero-padded hex), s (C
// string).
fn parse_print_format(spec: Option<&str>) -> Result<Option<char>> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let mut chars = spec.chars();
    let c = chars.next().ok_or_else(|| {
        DebuggerError::InvalidCommand("expected a format letter after '/'".to_string())
    })?;
    if chars.next().is_some() {
        return Err(DebuggerError::InvalidCommand(format!(
            "unsupported print format: /{} (expected a single letter)",
            spec
        )));
    }
    if !"xduotcfazs".contains(c) {
        return Err(DebuggerError::InvalidCommand(format!(
            "unknown print format: /{} (expected one of x d u o t c f a z s)",
            c
        )));
    }
    Ok(Some(c))
}

fn parse_set_print(args: &[&str]) -> Result<PrintOption> {
    let sub = args.first().map(|s| s.to_lowercase());
    let value = args.get(1).copied();

    let usage = || {
        DebuggerError::InvalidCommand(
            "set print requires: pretty on|off, address on|off, elements <n>|unlimited, or repeats <n>|unlimited"
                .to_string(),
        )
    };

    match sub.as_deref() {
        Some("pretty") => Ok(PrintOption::Pretty(parse_on_off(value).ok_or_else(usage)?)),
        Some("address") => Ok(PrintOption::Address(parse_on_off(value).ok_or_else(usage)?)),
        Some("elements") => Ok(PrintOption::Elements(parse_limit(value).ok_or_else(usage)?)),
        Some("repeats") => Ok(PrintOption::Repeats(parse_limit(value).ok_or_else(usage)?)),
        _ => Err(usage()),
    }
}

fn parse_on_off(value: Option<&str>) -> Option<bool> {
    match value?.to_lowercase().as_str() {
        "on" | "1" | "true" => Some(true),
        "off" | "0" | "false" => Some(false),
        _ => None,
    }
}

fn parse_limit(value: Option<&str>) -> Option<Option<usize>> {
    match value?.to_lowercase().as_str() {
        "unlimited" | "none" => Some(None),
        s => s.parse::<usize>().ok().map(Some),
    }
}

pub fn parse_address(s: &str) -> Result<usize> {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        usize::from_str_radix(&s[2..], 16)
            .map_err(|e| DebuggerError::Other(format!("invalid hex address: {}", e)))
    } else {
        s.parse::<usize>()
            .or_else(|_| {
                usize::from_str_radix(s, 16).map_err(|_| {
                    DebuggerError::Other(format!("invalid address: {}", s))
                })
            })
            .map_err(|e| e)
    }
}

pub fn help_text() -> &'static str {
    r#"Commands:
  r, run                       Start or restart the target
  c, continue                  Continue execution
  s, step                      Step into
  n, next                      Step over
  up, finish                   Run until the current function returns
  b, break <addr|symbol|file:line>  Set breakpoint
  d, delete <#>                 Delete breakpoint by number
  bl, breakpoints              List breakpoints
  watch <expr>                 Break when <expr> is written (hardware watchpoint)
  awatch, rwatch <expr>        Break when <expr> is read or written
  dw, deletewatch <#>           Delete watchpoint by number
  wl, watchpoints              List watchpoints
  threads                      List threads (current marked with *)
  thread <id>                  Switch the current thread (affects regs/bt/step/print/show)
  lock [id]                    Freeze all other threads, running only this one (current if omitted)
                                Caution: if a frozen thread holds a lock the
                                running one needs (your own, or an internal
                                OS/CRT one like the heap lock touched by
                                malloc/printf), continuing can hang forever.
  unlock                       Resume all threads frozen by 'lock'
  l, list [addr|symbol|file:line]  Show source (current position if omitted)
  regs, registers              Show registers
  x, memory <addr> [count]     Dump memory
  bt, tb, backtrace             Show call stack
  p, print[/fmt] <expr>        Evaluate a C expression
      fmt: x d u o t c f a z s (hex/dec/udec/oct/bin/char/float/addr/zpad/string)
  set <lvalue> = <expr>        Assign to a variable/register
      registers: $rax.. $r15, $rip, $rsp, $rbp, $eflags, $mxcsr,
      $mm0-7, $xmm0-15 (low 64 bits, $xmm0h-15h for the high half),
      $st0-7 (float-valued: assigns the value itself, not its bits)
  set print pretty on|off      Multi-line struct/array formatting
  set print address on|off     Annotate pointers with symbol info
  set print elements <n>|unlimited  Array/string print limit (default 200)
  set print repeats <n>|unlimited   Collapse repeated elements (default 10)
  show locals|args|globals     List variables and their values
  show print                   Show current 'set print' settings
  leak start|stop|report       Track malloc/calloc/free and report leaks
  h, help                      Show this help
  q, quit                      Quit
"#
}
