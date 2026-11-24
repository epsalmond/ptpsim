//! `camera-simctl`: CLI over the simulator's control HTTP. Synchronous, with
//! hand-written HTTP over std networking: it is a thin operator/test tool, not
//! a service. Request bodies are built from the service's own request types in
//! `camera-sim`, so the CLI and the service share one definition of each shape.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use camera_sim::{ActionRunRequest, AdvancePropertyRequest, StateOverlay};
use clap::{Args, Parser, Subcommand};
use serde::de::DeserializeOwned;

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
    /// List or run the manifest's control actions.
    Actions {
        #[command(subcommand)]
        command: ActionsCmd,
    },
    /// POST /control/advance-property: step one property to its next
    /// enumerated value.
    AdvanceProperty {
        /// Hex property code, such as 0x500D.
        code: String,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// GET the camera-observation/v1 export from the control endpoint.
    Observations {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        /// Return only records with an ordinal greater than this cursor.
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

#[derive(Subcommand)]
enum StateCmd {
    /// GET /state.
    Get {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// PATCH /state with a state overlay JSON document.
    Patch {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        #[command(flatten)]
        overlay: JsonInput,
    },
}

#[derive(Subcommand)]
enum ActionsCmd {
    /// GET /actions: the action catalog and its revision.
    List {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// POST /actions/<ID> with a JSON action request (catalogRevision, mode,
    /// role, parameters).
    Run {
        id: String,
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
        #[command(flatten)]
        request: JsonInput,
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

/// A JSON request body given inline or as a file. Exactly one is required.
#[derive(Args)]
#[group(required = true, multiple = false)]
struct JsonInput {
    /// Inline JSON.
    #[arg(long)]
    spec: Option<String>,
    /// Path to a file containing the JSON.
    #[arg(long, value_name = "PATH")]
    spec_file: Option<PathBuf>,
}

impl JsonInput {
    /// Parse the JSON into the service's request type `T`. Only the shape is
    /// checked here; the service performs every semantic check.
    fn parse<T: DeserializeOwned>(self, what: &str) -> Result<T> {
        let (text, source) = match (self.spec, self.spec_file) {
            (Some(spec), None) => (spec, "--spec".to_string()),
            (None, Some(path)) => (
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?,
                path.display().to_string(),
            ),
            _ => bail!("pass exactly one of --spec or --spec-file"),
        };
        serde_json::from_str(&text).with_context(|| format!("{source} is not a valid {what}"))
    }
}

fn main() -> Result<()> {
    let request = request_for(Cli::parse().cmd)?;
    let response = http(
        &request.control,
        request.method,
        &request.path,
        request.body.as_deref(),
    )?;
    // The response body goes to stdout unchanged, including a service error
    // body, so callers see the service's own message.
    println!("{}", response.body);
    if request.fail_on_http_error && !response.status.starts_with('2') {
        bail!("control endpoint answered HTTP {}", response.status);
    }
    Ok(())
}

struct Request {
    control: String,
    method: &'static str,
    path: String,
    body: Option<String>,
    /// Exit nonzero when the service answers with a non-2xx status. The
    /// original health, trace, shutdown, and fault commands keep their
    /// historical exit status of zero for any HTTP answer.
    fail_on_http_error: bool,
}

impl Request {
    fn new(control: String, method: &'static str, path: impl Into<String>) -> Self {
        Self {
            control,
            method,
            path: path.into(),
            body: None,
            fail_on_http_error: true,
        }
    }

    fn with_body(mut self, body: String) -> Self {
        self.body = Some(body);
        self
    }
}

fn request_for(cmd: Cmd) -> Result<Request> {
    Ok(match cmd {
        Cmd::Health { control } => Request {
            control,
            method: "GET",
            path: "/healthz".into(),
            body: None,
            fail_on_http_error: false,
        },
        Cmd::Trace { control, after } => Request {
            control,
            method: "GET",
            path: format!("/trace?after={after}"),
            body: None,
            fail_on_http_error: false,
        },
        Cmd::Shutdown { control } => Request {
            control,
            method: "POST",
            path: "/shutdown".into(),
            body: Some(String::new()),
            fail_on_http_error: false,
        },
        Cmd::Fault { command } => match command {
            FaultCmd::List { control } => Request {
                control,
                method: "GET",
                path: "/faults".into(),
                body: None,
                fail_on_http_error: false,
            },
            FaultCmd::Add { control, spec } => Request {
                control,
                method: "POST",
                path: "/faults".into(),
                body: Some(spec),
                fail_on_http_error: false,
            },
            FaultCmd::Delete { id, control } => Request {
                control,
                method: "DELETE",
                path: format!("/faults/{id}"),
                body: None,
                fail_on_http_error: false,
            },
            FaultCmd::Clear { control } => Request {
                control,
                method: "DELETE",
                path: "/faults".into(),
                body: None,
                fail_on_http_error: false,
            },
        },
        Cmd::State { command } => match command {
            StateCmd::Get { control } => Request::new(control, "GET", "/state"),
            StateCmd::Patch { control, overlay } => {
                let overlay: StateOverlay = overlay.parse("state overlay")?;
                Request::new(control, "PATCH", "/state").with_body(serde_json::to_string(&overlay)?)
            }
        },
        Cmd::Actions { command } => match command {
            ActionsCmd::List { control } => Request::new(control, "GET", "/actions"),
            ActionsCmd::Run {
                id,
                control,
                request,
            } => {
                let request: ActionRunRequest = request.parse("action request")?;
                Request::new(control, "POST", format!("/actions/{id}"))
                    .with_body(serde_json::to_string(&request)?)
            }
        },
        Cmd::AdvanceProperty { code, control } => {
            Request::new(control, "POST", "/control/advance-property")
                .with_body(serde_json::to_string(&AdvancePropertyRequest { code })?)
        }
        Cmd::Observations { control, after } => {
            Request::new(control, "GET", format!("/observations?after={after}"))
        }
        Cmd::ConnectionLifecycles { control } => {
            Request::new(control, "GET", "/connection-lifecycles")
        }
        Cmd::Callbacks { command } => match command {
            // The service reads this body untyped, so there is no shared type
            // to reuse for its single `url` field.
            CallbacksCmd::Add { url, control } => Request::new(control, "POST", "/callbacks")
                .with_body(serde_json::json!({ "url": url }).to_string()),
        },
    })
}

struct HttpResponse {
    /// Status code and reason phrase, such as `400 Bad Request`.
    status: String,
    body: String,
}

fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> Result<HttpResponse> {
    let mut s = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    let extra = match body {
        Some(body) => format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ),
        None => String::new(),
    };
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}Connection: close\r\n\r\n{body}"
    )?;
    let mut out = String::new();
    s.read_to_string(&mut out)?;
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_once(' '))
        .map(|(_, status)| status.to_string())
        .with_context(|| format!("malformed HTTP response from {addr}"))?;
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> Request {
        request_for(Cli::try_parse_from(args).unwrap().cmd).unwrap()
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
    fn control_commands_map_to_http_requests() {
        let get = request(&["camera-simctl", "state", "get"]);
        assert_eq!((get.method, get.path.as_str()), ("GET", "/state"));
        assert!(get.fail_on_http_error);

        let patch = request(&[
            "camera-simctl",
            "state",
            "patch",
            "--spec",
            r#"{"props":{"0x500d":125000}}"#,
        ]);
        assert_eq!((patch.method, patch.path.as_str()), ("PATCH", "/state"));
        let body: StateOverlay = serde_json::from_str(&patch.body.unwrap()).unwrap();
        assert_eq!(body.props["0x500d"], 125000);

        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"phase":"liveView"}"#).unwrap();
        let path = file.path().to_str().unwrap();
        let patch = request(&["camera-simctl", "state", "patch", "--spec-file", path]);
        let body: StateOverlay = serde_json::from_str(&patch.body.unwrap()).unwrap();
        assert_eq!(body.phase.as_deref(), Some("liveView"));

        let list = request(&["camera-simctl", "actions", "list"]);
        assert_eq!((list.method, list.path.as_str()), ("GET", "/actions"));

        let run = request(&[
            "camera-simctl",
            "actions",
            "run",
            "shutter",
            "--spec",
            r#"{"catalogRevision":"r1","mode":"shooting/stills","role":"responder"}"#,
        ]);
        assert_eq!(
            (run.method, run.path.as_str()),
            ("POST", "/actions/shutter")
        );
        let body: ActionRunRequest = serde_json::from_str(&run.body.unwrap()).unwrap();
        assert_eq!(body.catalog_revision, "r1");

        let advance = request(&["camera-simctl", "advance-property", "0x500D"]);
        assert_eq!(
            (
                advance.method,
                advance.path.as_str(),
                advance.body.as_deref()
            ),
            (
                "POST",
                "/control/advance-property",
                Some(r#"{"code":"0x500D"}"#)
            )
        );

        let observations = request(&["camera-simctl", "observations", "--after", "7"]);
        assert_eq!(
            (observations.method, observations.path.as_str()),
            ("GET", "/observations?after=7")
        );

        let lifecycles = request(&["camera-simctl", "connection-lifecycles"]);
        assert_eq!(
            (lifecycles.method, lifecycles.path.as_str()),
            ("GET", "/connection-lifecycles")
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
    fn json_input_requires_exactly_one_source() {
        assert!(Cli::try_parse_from(["camera-simctl", "state", "patch"]).is_err());
        assert!(Cli::try_parse_from([
            "camera-simctl",
            "state",
            "patch",
            "--spec",
            "{}",
            "--spec-file",
            "overlay.json",
        ])
        .is_err());
    }

    #[test]
    fn malformed_state_overlay_is_reported_before_sending() {
        let cli = Cli::try_parse_from([
            "camera-simctl",
            "state",
            "patch",
            "--spec",
            r#"{"unknownField":1}"#,
        ])
        .unwrap();
        let error = request_for(cli.cmd).err().unwrap();
        assert!(format!("{error:#}").contains("unknown field `unknownField`"));
    }
}
