mod app;
mod builder;
mod config;
mod host;
mod report;
mod solution;

use std::fmt;

#[derive(Debug)]
enum AppError {
    Usage(String),
    Runtime(String),
    Exit(i32),
}

type AppResult<T> = Result<T, AppError>;

impl AppError {
    fn code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Runtime(_) => 1,
            Self::Exit(code) => *code,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Runtime(message) => f.write_str(message),
            Self::Exit(_) => Ok(()),
        }
    }
}

fn main() {
    if let Err(error) = app::run() {
        if !matches!(error, AppError::Exit(_)) {
            eprintln!("error: {error}");
        }
        std::process::exit(error.code());
    }
}
