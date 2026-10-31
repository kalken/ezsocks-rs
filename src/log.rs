//! Minimal leveled logging to stderr (the journal, under systemd).

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Warning = 30,
    Error = 40,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "INFO" => Some(Level::Info),
            "WARNING" => Some(Level::Warning),
            "ERROR" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Warning as u8);

pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

#[inline]
pub fn enabled(level: Level) -> bool {
    level as u8 >= LEVEL.load(Ordering::Relaxed)
}

pub fn write(level: Level, name: &str, args: std::fmt::Arguments) {
    use std::io::Write;
    // One write per line, so lines from different threads don't interleave.
    let line = format!("{:<8} {} {}\n", level.name(), name, args);
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($level:expr, $name:expr, $($arg:tt)+) => {
        if $crate::log::enabled($level) {
            $crate::log::write($level, $name, format_args!($($arg)+));
        }
    };
}

#[macro_export]
macro_rules! debug { ($name:expr, $($arg:tt)+) => { $crate::log!($crate::log::Level::Debug, $name, $($arg)+) }; }
#[macro_export]
macro_rules! info { ($name:expr, $($arg:tt)+) => { $crate::log!($crate::log::Level::Info, $name, $($arg)+) }; }
#[macro_export]
macro_rules! warning { ($name:expr, $($arg:tt)+) => { $crate::log!($crate::log::Level::Warning, $name, $($arg)+) }; }
#[macro_export]
macro_rules! error { ($name:expr, $($arg:tt)+) => { $crate::log!($crate::log::Level::Error, $name, $($arg)+) }; }
