//! Log contract per docs/rust-rewrite/config-auth-boundary.md §5:
//! `[LEVEL] message` on stdout for every level; threshold from
//! MOUSEHOLE_LOG_LEVEL; ANSI on the prefix only when stdout is a TTY and
//! NO_COLOR is absent (present-but-empty still disables).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    Debug = 2,
    Info = 3,
    Warn = 4,
    Error = 5,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Debug => "\x1b[90m",
            Self::Info => "\x1b[36m",
            Self::Warn => "\x1b[33m",
            Self::Error => "\x1b[31m",
        }
    }
}

static THRESHOLD: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

pub fn set_level(level: LogLevel) {
    THRESHOLD.store(level as u8, Ordering::Relaxed);
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub fn log(level: LogLevel, message: &str) {
    if (level as u8) < THRESHOLD.load(Ordering::Relaxed) {
        return;
    }
    if use_color() {
        println!("{}[{}]\x1b[0m {}", level.color(), level.label(), message);
    } else {
        println!("[{}] {}", level.label(), message);
    }
}

pub fn debug(message: &str) {
    log(LogLevel::Debug, message);
}
pub fn info(message: &str) {
    log(LogLevel::Info, message);
}
pub fn warn(message: &str) {
    log(LogLevel::Warn, message);
}
pub fn error(message: &str) {
    log(LogLevel::Error, message);
}
