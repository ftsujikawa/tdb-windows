mod breakpoint;
mod commands;
mod debugger;
mod disasm;
mod error;
mod eval;
mod process;
mod symbols;
mod watchpoint;

use anyhow::Result;
use clap::Parser;
use debugger::Debugger;

#[derive(Parser, Debug)]
#[command(name = "tdb-windows", about = "Tiny debugger for C programs on Windows")]
struct Args {
    #[arg(help = "Path to the target executable and optional arguments")]
    target: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.target.is_empty() {
        anyhow::bail!("Target executable is required");
    }

    // Without our own handler, Ctrl+C just kills tdb-windows immediately
    // (default console behavior); this lets us forward it to the debuggee
    // instead (see process::interrupt).
    process::install_interrupt_handler()?;
    // Best-effort: lets OpenThread reach threads that would otherwise deny
    // GetContext/SetContext even to the active debugger (see the function
    // doc comment). Requires running elevated to have any effect.
    process::enable_debug_privilege();

    let target = args.target.join(" ");
    let mut debugger = Debugger::new(target);
    debugger.run_repl()?;
    Ok(())
}
