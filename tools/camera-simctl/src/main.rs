//! `camera-simctl` — CLI over the simulator's control HTTP. Std-only and
//! synchronous: it is a thin operator/test tool, not a service.

use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "camera-simctl", about = "Control a ptpsim instance")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// GET /healthz on the control endpoint.
    Health {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// GET the sequence-numbered lifecycle trace from the control endpoint.
    Trace {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        /// Return only events with a sequence greater than this cursor.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// POST /shutdown on the control endpoint.
    Shutdown {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Health { control } => {
            println!("{}", http(&control, "GET", "/healthz")?);
        }
        Cmd::Trace { control, after } => {
            println!(
                "{}",
                http(&control, "GET", &format!("/trace?after={after}"))?
            );
        }
        Cmd::Shutdown { control } => {
            println!("{}", http(&control, "POST", "/shutdown")?);
        }
    }
    Ok(())
}

fn http(addr: &str, method: &str, path: &str) -> Result<String> {
    let mut s = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    let extra = if method == "POST" {
        "Content-Length: 0\r\n"
    } else {
        ""
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}Connection: close\r\n\r\n"
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out.rsplit("\r\n\r\n").next().unwrap_or("").to_string())
}
