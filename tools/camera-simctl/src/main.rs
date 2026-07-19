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
    /// Manage occurrence-scoped command-channel faults.
    Fault {
        #[command(subcommand)]
        command: FaultCmd,
    },
}

#[derive(Subcommand)]
enum FaultCmd {
    /// GET /faults.
    List {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// POST a JSON fault specification to /faults.
    Add {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        #[arg(long)]
        spec: String,
    },
    /// DELETE one fault by server-assigned id.
    Delete {
        id: u64,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// DELETE every installed fault.
    Clear {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
}

fn main() -> Result<()> {
    let request = request_for(Cli::parse().cmd);
    println!(
        "{}",
        http(
            &request.control,
            request.method,
            &request.path,
            request.body.as_deref()
        )?
    );
    Ok(())
}

struct Request {
    control: String,
    method: &'static str,
    path: String,
    body: Option<String>,
}

fn request_for(cmd: Cmd) -> Request {
    match cmd {
        Cmd::Health { control } => Request {
            control,
            method: "GET",
            path: "/healthz".into(),
            body: None,
        },
        Cmd::Trace { control, after } => Request {
            control,
            method: "GET",
            path: format!("/trace?after={after}"),
            body: None,
        },
        Cmd::Shutdown { control } => Request {
            control,
            method: "POST",
            path: "/shutdown".into(),
            body: Some(String::new()),
        },
        Cmd::Fault { command } => match command {
            FaultCmd::List { control } => Request {
                control,
                method: "GET",
                path: "/faults".into(),
                body: None,
            },
            FaultCmd::Add { control, spec } => Request {
                control,
                method: "POST",
                path: "/faults".into(),
                body: Some(spec),
            },
            FaultCmd::Delete { id, control } => Request {
                control,
                method: "DELETE",
                path: format!("/faults/{id}"),
                body: None,
            },
            FaultCmd::Clear { control } => Request {
                control,
                method: "DELETE",
                path: "/faults".into(),
                body: None,
            },
        },
    }
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> Result<String> {
    let mut s = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    let body = body.unwrap_or("");
    let extra = if method == "POST" {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    } else {
        String::new()
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}Connection: close\r\n\r\n{body}"
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    Ok(out.rsplit("\r\n\r\n").next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> Request {
        request_for(Cli::try_parse_from(args).unwrap().cmd)
    }

    #[test]
    fn fault_commands_map_to_http_requests() {
        let list = request(&["camera-simctl", "fault", "list", "--control", "host:1"]);
        assert_eq!(
            (list.method, list.path.as_str(), list.body),
            ("GET", "/faults", None)
        );

        let add = request(&[
            "camera-simctl",
            "fault",
            "add",
            "--spec",
            r#"{"operation":"0x1015","mutation":{"type":"suppress","stage":"data"}}"#,
        ]);
        assert_eq!(add.method, "POST");
        assert_eq!(add.path, "/faults");
        assert!(add.body.unwrap().contains("0x1015"));

        let delete = request(&["camera-simctl", "fault", "delete", "42"]);
        assert_eq!(
            (delete.method, delete.path.as_str()),
            ("DELETE", "/faults/42")
        );

        let clear = request(&["camera-simctl", "fault", "clear"]);
        assert_eq!((clear.method, clear.path.as_str()), ("DELETE", "/faults"));
    }
}
