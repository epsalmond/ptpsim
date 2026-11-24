//! `camera-simctl`: CLI over the simulator's control HTTP. Synchronous, with
//! hand-written HTTP over std networking: it is a thin operator/test tool, not
//! a service. JSON request bodies are checked against the service's own
//! request types in `camera-sim`, so the CLI and the service share one
//! definition of each shape.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;

use anyhow::{Context, Result};
use camera_sim::{ActionRunRequest, AdvancePropertyRequest, StateOverlay};
use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;

#[derive(Parser)]
#[command(
    name = "camera-simctl",
    about = "Control a ptpsim instance",
    after_help = "Exit status: 0 for a 2xx response, 1 when the request cannot be \
                  sent or the response cannot be read, 2 for invalid arguments, and \
                  3 when the control server returns a non-2xx status. The response \
                  body is printed in every case where one is received."
)]
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
    /// GET /connection-lifecycles.
    ConnectionLifecycles {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// Manage observers that receive camera-state snapshots.
    Callbacks {
        #[command(subcommand)]
        command: CallbacksCmd,
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
        /// Inline JSON state overlay.
        #[arg(
            long,
            value_parser = json_text::<StateOverlay>,
            required_unless_present = "overlay_file",
            conflicts_with = "overlay_file"
        )]
        overlay: Option<String>,
        /// Path to a JSON state overlay file.
        #[arg(long, value_name = "PATH", value_parser = json_file::<StateOverlay>)]
        overlay_file: Option<String>,
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
        #[arg(
            long,
            value_parser = json_text::<ActionRunRequest>,
            required_unless_present = "request_file",
            conflicts_with = "request_file"
        )]
        request: Option<String>,
        /// Path to a file containing the request body.
        #[arg(long, value_name = "PATH", value_parser = json_file::<ActionRunRequest>)]
        request_file: Option<String>,
    },
}

#[derive(Subcommand)]
enum CallbacksCmd {
    /// POST /callbacks: register an http:// URL that receives a JSON
    /// camera-state snapshot on every change.
    Add {
        #[arg(long)]
        url: String,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
}

/// Check that inline JSON has the shape of the service's request type `T`,
/// then keep the original text so it is sent unchanged. A shape error is a
/// usage error; the service performs every semantic check.
fn json_text<T: DeserializeOwned>(text: &str) -> Result<String, String> {
    serde_json::from_str::<T>(text).map_err(|error| error.to_string())?;
    Ok(text.to_string())
}

/// Read a JSON file and check it as `json_text` does.
fn json_file<T: DeserializeOwned>(path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
    json_text::<T>(&text).map_err(|error| format!("{path}: {error}"))
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

/// Exit status when the control server answers with a non-2xx status. Clap
/// uses 2 for usage errors and a returned error exits 1.
const EXIT_HTTP_STATUS: u8 = 3;

fn main() -> Result<ExitCode> {
    let request = request_for(Cli::parse().cmd);
    let response = parse_response(&http(
        &request.control,
        request.method,
        &request.path,
        request.body.as_deref(),
    )?)?;
    println!("{}", response.body);
    let code = exit_code_for(response.status);
    if code != 0 {
        eprintln!(
            "camera-simctl: control server returned HTTP {}",
            response.status
        );
    }
    Ok(ExitCode::from(code))
}

fn exit_code_for(status: u16) -> u8 {
    if (200..300).contains(&status) {
        0
    } else {
        EXIT_HTTP_STATUS
    }
}

struct Response {
    status: u16,
    body: String,
}

fn parse_response(raw: &str) -> Result<Response> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .context("control response has no header terminator")?;
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .with_context(|| format!("control response has invalid status line {status_line:?}"))?;
    Ok(Response {
        status,
        body: body.to_string(),
    })
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
            StateCmd::Patch {
                control,
                overlay,
                overlay_file,
            } => Request {
                control,
                method: "PATCH",
                path: "/state".into(),
                // Clap requires exactly one of the two.
                body: overlay.or(overlay_file),
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
                request_file,
            } => Request {
                control,
                method: "POST",
                path: format!("/actions/{id}"),
                // Clap requires exactly one of the two.
                body: request.or(request_file),
            },
        },
        Cmd::AdvanceProperty { code, control } => {
            let request = AdvancePropertyRequest {
                code: format!("{code:#06x}"),
            };
            Request {
                control,
                method: "POST",
                path: "/control/advance-property".into(),
                body: Some(serde_json::json!(request).to_string()),
            }
        }
        Cmd::Observations { control, after } => Request {
            control,
            method: "GET",
            path: format!("/observations?after={after}"),
            body: None,
        },
        Cmd::ConnectionLifecycles { control } => Request {
            control,
            method: "GET",
            path: "/connection-lifecycles".into(),
            body: None,
        },
        Cmd::Callbacks { command } => match command {
            // The service reads this body untyped, so there is no shared type
            // to reuse for its single `url` field.
            CallbacksCmd::Add { url, control } => Request {
                control,
                method: "POST",
                path: "/callbacks".into(),
                body: Some(serde_json::json!({ "url": url }).to_string()),
            },
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
    Ok(out)
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

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"phase":"liveView"}"#).unwrap();
        let path = file.path().to_str().unwrap();
        let patch = request(&["camera-simctl", "state", "patch", "--overlay-file", path]);
        assert_eq!(patch.body.as_deref(), Some(r#"{"phase":"liveView"}"#));
    }

    #[test]
    fn json_bodies_need_exactly_one_source_with_the_service_shape() {
        for args in [
            &["camera-simctl", "state", "patch"][..],
            &[
                "camera-simctl",
                "state",
                "patch",
                "--overlay",
                "{}",
                "--overlay-file",
                "overlay.json",
            ],
            &["camera-simctl", "actions", "run", "shutter"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
        let error = Cli::try_parse_from([
            "camera-simctl",
            "state",
            "patch",
            "--overlay",
            r#"{"unknownField":1}"#,
        ])
        .err()
        .unwrap();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(error.to_string().contains("unknown field `unknownField`"));
    }

    #[test]
    fn lifecycle_and_callback_commands_map_to_http_requests() {
        let lifecycles = request(&["camera-simctl", "connection-lifecycles"]);
        assert_eq!(
            (lifecycles.method, lifecycles.path.as_str(), lifecycles.body),
            ("GET", "/connection-lifecycles", None)
        );

        let callback = request(&[
            "camera-simctl",
            "callbacks",
            "add",
            "--url",
            "http://127.0.0.1:9/state",
        ]);
        assert_eq!(
            (
                callback.method,
                callback.path.as_str(),
                callback.body.as_deref()
            ),
            (
                "POST",
                "/callbacks",
                Some(r#"{"url":"http://127.0.0.1:9/state"}"#)
            )
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
    fn non_success_status_fails_and_keeps_the_body() {
        let ok =
            parse_response("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").unwrap();
        assert_eq!((ok.status, ok.body.as_str()), (200, r#"{"ok":true}"#));
        assert_eq!(exit_code_for(ok.status), 0);

        for (raw, status) in [
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 15\r\n\r\n{\"error\":\"bad\"}",
                400,
            ),
            ("HTTP/1.1 404 Not Found\r\n\r\n", 404),
            (
                "HTTP/1.1 500 Internal Server Error\r\n\r\n{\"error\":\"x\"}",
                500,
            ),
        ] {
            let response = parse_response(raw).unwrap();
            assert_eq!(response.status, status);
            assert_eq!(exit_code_for(response.status), EXIT_HTTP_STATUS, "{raw}");
        }
        assert_eq!(
            parse_response("HTTP/1.1 400 Bad Request\r\n\r\n{\"error\":\"bad\"}")
                .unwrap()
                .body,
            r#"{"error":"bad"}"#
        );
        assert_ne!(EXIT_HTTP_STATUS, 2, "2 is clap's usage-error status");
    }

    #[test]
    fn response_body_starts_after_the_first_blank_line() {
        let response = parse_response("HTTP/1.1 200 OK\r\n\r\nline\r\n\r\nmore").unwrap();
        assert_eq!(response.body, "line\r\n\r\nmore");
    }

    #[test]
    fn malformed_response_is_an_error() {
        for raw in ["", "garbage", "HTTP/1.1 abc\r\n\r\n", "HTTP/1.1 200 OK"] {
            assert!(parse_response(raw).is_err(), "{raw:?}");
        }
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
