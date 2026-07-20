use thiserror::Error;
use windows::core::Error as WindowsError;

pub type Result<T> = std::result::Result<T, DebuggerError>;

#[derive(Error, Debug)]
pub enum DebuggerError {
    #[error("Windows API error: {0}")]
    Windows(#[from] WindowsError),

    #[error("OS error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid command: {0}")]
    InvalidCommand(String),

    #[error("Process not attached")]
    NotAttached,

    #[error("No active breakpoint")]
    NoBreakpoint,

    #[error("No active watchpoint")]
    NoWatchpoint,

    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("No such process: {0}")]
    NoSuchProcess(u32),

    #[error("{0}")]
    Other(String),
}
