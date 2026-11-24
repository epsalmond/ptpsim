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
    /// Read or patch the simulated camera state.
    State {
        #[command(subcommand)]
        command: StateCmd,
    },
    /// List or run manifest-backed camera actions.
    Actions {
        #[command(subcommand)]
        command: ActionsCmd,
    },
    /// POST /control/advance-property: step a property to its next enum value.
    AdvanceProperty {
        /// Property code in hex, with or without a 0x prefix (for example 0x500D).
        #[arg(value_parser = parse_property_code)]
        code: u16,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// GET canonical observation records from the control endpoint.
    Observations {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        /// Return only records with a cursor greater than this value.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
}

#[derive(Subcommand)]
enum StateCmd {
    /// GET /state.
    Get {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// PATCH /state with a JSON state overlay.
    Patch {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        #[arg(long)]
        overlay: String,
    },
}

#[derive(Subcommand)]
enum ActionsCmd {
    /// GET /actions.
    List {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// POST a JSON action request to /actions/<id>.
    Run {
        id: String,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        /// Request body with catalogRevision, mode, role, and optional parameters.
        #[arg(long)]
        request: String,
    },
}

fn parse_property_code(value: &str) -> Result<u16, String> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    // from_str_radix accepts a leading '+', which is not a hex digit.
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("'{value}' is not a hexadecimal property code"));
    }
    u16::from_str_radix(hex, 16).map_err(|_| format!("'{value}' does not fit in 16 bits"))
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
        Cmd::State { command } => match command {
            StateCmd::Get { control } => Request {
                control,
                method: "GET",
                path: "/state".into(),
                body: None,
            },
            StateCmd::Patch { control, overlay } => Request {
                control,
                method: "PATCH",
                path: "/state".into(),
                body: Some(overlay),
            },
        },
        Cmd::Actions { command } => match command {
            ActionsCmd::List { control } => Request {
                control,
                method: "GET",
                path: "/actions".into(),
                body: None,
            },
            ActionsCmd::Run {
                id,
                control,
                request,
            } => Request {
                control,
                method: "POST",
                path: format!("/actions/{id}"),
                body: Some(request),
            },
        },
        Cmd::AdvanceProperty { code, control } => Request {
            control,
            method: "POST",
            path: "/control/advance-property".into(),
            body: Some(format!(r#"{{"code":"{code:#06x}"}}"#)),
        },
        Cmd::Observations { control, after } => Request {
            control,
            method: "GET",
            path: format!("/observations?after={after}"),
            body: None,
        },
    }
}

/// The control server reads a request body only when Content-Length is
/// present, so every request that carries a body declares it.
fn render_request(method: &str, path: &str, body: Option<&str>) -> String {
    let (headers, body) = match body {
        Some(body) => (
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ),
            body,
        ),
        None => (String::new(), ""),
    };
    format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{headers}Connection: close\r\n\r\n{body}")
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> Result<String> {
    let mut s = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    s.write_all(render_request(method, path, body).as_bytes())?;
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

    #[test]
    fn state_commands_map_to_http_requests() {
        let get = request(&["camera-simctl", "state", "get", "--control", "host:1"]);
        assert_eq!(
            (
                get.control.as_str(),
                get.method,
                get.path.as_str(),
                get.body
            ),
            ("host:1", "GET", "/state", None)
        );

        let overlay = r#"{"props":{"0x500F":2000}}"#;
        let patch = request(&["camera-simctl", "state", "patch", "--overlay", overlay]);
        assert_eq!(
            (patch.method, patch.path.as_str(), patch.body.as_deref()),
            ("PATCH", "/state", Some(overlay))
        );
    }

    #[test]
    fn action_commands_map_to_http_requests() {
        let list = request(&["camera-simctl", "actions", "list"]);
        assert_eq!(
            (list.method, list.path.as_str(), list.body),
            ("GET", "/actions", None)
        );

        let body = r#"{"catalogRevision":"r1","mode":"shooting/stills","role":"responder"}"#;
        let run = request(&[
            "camera-simctl",
            "actions",
            "run",
            "shutter",
            "--request",
            body,
            "--control",
            "host:1",
        ]);
        assert_eq!(
            (
                run.control.as_str(),
                run.method,
                run.path.as_str(),
                run.body.as_deref()
            ),
            ("host:1", "POST", "/actions/shutter", Some(body))
        );
    }

    #[test]
    fn advance_property_maps_to_http_request() {
        let advance = request(&["camera-simctl", "advance-property", "500D"]);
        assert_eq!(
            (
                advance.method,
                advance.path.as_str(),
                advance.body.as_deref()
            ),
            (
                "POST",
                "/control/advance-property",
                Some(r#"{"code":"0x500d"}"#)
            )
        );

        let prefixed = request(&["camera-simctl", "advance-property", "0xD209"]);
        assert_eq!(prefixed.body.as_deref(), Some(r#"{"code":"0xd209"}"#));

        for invalid in ["0x10000", "+500D", "iso", "", "0x", "0x\"5005"] {
            assert!(
                Cli::try_parse_from(["camera-simctl", "advance-property", invalid]).is_err(),
                "{invalid:?} must be rejected before a request is sent"
            );
        }
    }

    #[test]
    fn observations_command_maps_to_cursor_request() {
        let first = request(&["camera-simctl", "observations"]);
        assert_eq!(
            (first.method, first.path.as_str(), first.body),
            ("GET", "/observations?after=0", None)
        );

        let next = request(&["camera-simctl", "observations", "--after", "17"]);
        assert_eq!(next.path, "/observations?after=17");
    }

    #[test]
    fn request_bodies_carry_content_length_for_every_method() {
        let patch = render_request("PATCH", "/state", Some(r#"{"a":1}"#));
        assert!(patch.starts_with("PATCH /state HTTP/1.1\r\n"), "{patch}");
        assert!(
            patch.contains("Content-Type: application/json\r\n"),
            "{patch}"
        );
        assert!(patch.contains("Content-Length: 7\r\n"), "{patch}");
        assert!(patch.ends_with("\r\n\r\n{\"a\":1}"), "{patch}");

        let shutdown = render_request("POST", "/shutdown", Some(""));
        assert!(shutdown.contains("Content-Length: 0\r\n"), "{shutdown}");

        let get = render_request("GET", "/state", None);
        assert!(!get.contains("Content-Length"), "{get}");
    }
}
