//! The shipped GFX100 II startup-state fixture for an empty memory card slot 1
//! (#352) starts the real service binary with that simulator state active. This
//! runs the same `--startup-state` path an operator uses, against the shipped
//! consolidated manifest, so renaming the state in the manifest data or the
//! fixture fails here.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const FIXTURE: &str = "packages/fixtures/startup-state/gfx100ii-memory-card-slot1-empty.yaml";
const MANIFEST: &str = "packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml";

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// Kills the service if the test ends early.
struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Read JSON log lines until the service reports its control address.
fn control_address(service: &mut Service) -> SocketAddr {
    let stdout = service.0.stdout.take().expect("piped stdout");
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("service log line");
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if record["fields"]["message"] == "ptpsim listening" {
            return record["fields"]["control"]
                .as_str()
                .expect("control address field")
                .parse()
                .expect("control address");
        }
    }
    panic!("the service exited before it listened");
}

fn get_state(control: SocketAddr) -> serde_json::Value {
    let mut stream = TcpStream::connect(control).unwrap();
    stream
        .write_all(b"GET /state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap();
    serde_json::from_str(body).unwrap()
}

#[test]
fn shipped_fixture_starts_the_gfx100ii_with_slot_one_empty() {
    let media = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(media.path().join("DCIM/100_FUJI")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_camera-sim-service"))
        .arg("--manifest")
        .arg(repo(MANIFEST))
        .arg("--startup-state")
        .arg(repo(FIXTURE))
        .arg("--media-root")
        .arg(media.path())
        .args(["--profile", "fuji/gfx100ii", "--connection", "app"])
        .args(["--command-bind", "127.0.0.1:0"])
        .args(["--event-bind", "127.0.0.1:0"])
        .args(["--liveview-bind", "127.0.0.1:0"])
        .args(["--control-bind", "127.0.0.1:0"])
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("service starts");
    let mut service = Service(child);
    let control = control_address(&mut service);

    let state = get_state(control);
    assert_eq!(state["states"], serde_json::json!(["memoryCardSlot1Empty"]));
    assert_eq!(state["session_open"], false);
}
