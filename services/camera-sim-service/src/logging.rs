use std::io::IsTerminal;

use clap::ValueEnum;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum LogFormat {
    Auto,
    Json,
    Compact,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

pub fn init(format: LogFormat, color: ColorChoice) {
    let terminal = std::io::stdout().is_terminal();
    let format = match format {
        LogFormat::Auto if terminal => LogFormat::Compact,
        LogFormat::Auto => LogFormat::Json,
        explicit => explicit,
    };
    let ansi = match color {
        ColorChoice::Auto => terminal && std::env::var_os("NO_COLOR").is_none(),
        ColorChoice::Always => true,
        ColorChoice::Never => false,
    };

    match format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_ansi(false)
            .json()
            .init(),
        LogFormat::Compact | LogFormat::Auto => tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_ansi(ansi)
            .compact()
            .init(),
    }
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}
