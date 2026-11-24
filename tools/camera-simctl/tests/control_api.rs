//! Drive the `camera-simctl` binary against an in-process simulator service and
//! check that each control subcommand has its effect on the running service.

use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use camera_sim_service::{Config, Server};

const PROFILE: &str = "fuji/gfx100ii/fw0230";

/// A running service on ephemeral loopback ports. Dropping it shuts the
/// service down and removes its temporary media root.
struct Service {
    control: SocketAddr,
    runtime: tokio::runtime::Runtime,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    _media: tempfile::TempDir,
}

impl Service {
    fn start(connection: &str) -> Self {
        let media = tempfile::tempdir().unwrap();
        let folder = media.path().join("DCIM/100_FUJI");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("DSCF0001.JPG"), b"\xFF\xD8HELLOJPEG\xFF\xD9").unwrap();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
        let manifest_yaml = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest.display()));
        // The app connection has event and live-view sockets; the wireless
        // tether connection serves the shutter action that enqueues objects.
        let app = connection == "app";
        let ephemeral = || Some("127.0.0.1:0".parse().unwrap());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (control, shutdown, task) = runtime.block_on(async {
            let server = Server::bind(Config {
                instance_id: "simctl-test".into(),
                profile: PROFILE.into(),
                connection: connection.into(),
                manifest_yaml,
                media_root: media.path().to_path_buf(),
                command_bind: Some("127.0.0.1:0".parse().unwrap()),
                liveview_bind: if app { ephemeral() } else { None },
                event_bind: if app { ephemeral() } else { None },
                knock_bind: None,
                pcss_init_fails: 0,
                pcss_shutter_enqueue_count: if app { 0 } else { 1 },
                control_bind: "127.0.0.1:0".parse().unwrap(),
                liveview_dir: None,
                state_callback: None,
            })
            .await
            .unwrap();
            let control = server.control_addr();
            let (tx, rx) = tokio::sync::oneshot::channel();
            (control, tx, tokio::spawn(server.run(rx)))
        });
        Self {
            control,
            runtime,
            shutdown: Some(shutdown),
            task: Some(task),
            _media: media,
        }
    }

    /// Run `camera-simctl <args> --control <this service>`.
    fn simctl(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_camera-simctl"))
            .args(args)
            .args(["--control", &self.control.to_string()])
            .output()
            .expect("run camera-simctl")
    }

    /// Run a subcommand that must succeed and return its stdout as JSON.
    fn simctl_json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.simctl(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{args:?} failed: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|error| panic!("{args:?} printed non-JSON ({error}): {stdout}"))
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = self.runtime.block_on(task);
        }
    }
}

#[test]
fn state_patch_applies_inline_json_and_json_files() {
    let service = Service::start("app");

    let patched =
        service.simctl_json(&["state", "patch", "--spec", r#"{"props":{"0x500d":125000}}"#]);
    assert_eq!(patched["ok"], true, "{patched}");
    let state = service.simctl_json(&["state", "get"]);
    assert_eq!(state["props"]["0x500d"], 125000, "{state}");

    let overlay = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(overlay.path(), r#"{"phase":"liveView"}"#).unwrap();
    let path = overlay.path().to_str().unwrap();
    let patched = service.simctl_json(&["state", "patch", "--spec-file", path]);
    assert_eq!(patched["applied"]["phase"], true, "{patched}");
    let state = service.simctl_json(&["state", "get"]);
    assert_eq!(state["phase"], "liveView", "{state}");
}

#[test]
fn state_patch_surfaces_service_validation_errors_with_nonzero_exit() {
    let service = Service::start("app");

    let output = service.simctl(&["state", "patch", "--spec", r#"{"profile":"other/body"}"#]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "rejected patch must fail: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "startup state profile 'other/body' does not match running profile '{PROFILE}'"
        )),
        "service error must be printed verbatim: {stdout}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("400"),
        "stderr names the HTTP status"
    );
}

#[test]
fn advance_property_moves_to_the_next_enum_value() {
    let service = Service::start("app");
    service.simctl_json(&["state", "patch", "--spec", r#"{"props":{"0x500d":125000}}"#]);

    let advanced = service.simctl_json(&["advance-property", "0x500D"]);
    assert_eq!(advanced["code"], "0x500d", "{advanced}");
    let state = service.simctl_json(&["state", "get"]);
    assert_eq!(state["props"]["0x500d"], 157490, "{state}");

    let output = service.simctl(&["advance-property", "not-a-code"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("invalid property code 'not-a-code' in advance-property request"));
}

#[test]
fn actions_list_run_and_observations_round_trip() {
    let service = Service::start("wireless-tether");

    let catalog = service.simctl_json(&["actions", "list"]);
    let revision = catalog["revision"].as_str().expect("catalog revision");
    assert!(
        catalog["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["connection"] == "wireless-tether"
                && action["actionId"] == "shutter")
    );

    let request = serde_json::json!({
        "catalogRevision": revision,
        "mode": "shooting/stills",
        "role": "responder",
        "parameters": [],
    })
    .to_string();
    let ran = service.simctl_json(&["actions", "run", "shutter", "--spec", &request]);
    assert_eq!(ran["affectedObjects"], 1, "{ran}");

    let wrong_mode = serde_json::json!({
        "catalogRevision": revision,
        "mode": "image-transfer",
        "role": "responder",
    })
    .to_string();
    let rejected = service.simctl(&["actions", "run", "shutter", "--spec", &wrong_mode]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stdout).contains("wrongMode"));

    let export = service.simctl_json(&["observations"]);
    assert_eq!(export["schema"], "camera-observation/v1");
    let records = export["records"].as_array().unwrap();
    assert!(records.iter().any(|record| {
        record["kind"] == "actionInvocation"
            && record["actionId"] == "shutter"
            && record["outcome"] == "succeeded"
    }));
    let cursor = export["cursor"].as_u64().unwrap().to_string();
    let resumed = service.simctl_json(&["observations", "--after", &cursor]);
    assert_eq!(resumed["records"].as_array().unwrap().len(), 0, "{resumed}");
}

#[test]
fn connection_lifecycles_returns_the_service_document() {
    let service = Service::start("app");
    let lifecycles = service.simctl_json(&["connection-lifecycles"]);
    assert!(lifecycles.is_object(), "{lifecycles}");
}

#[test]
fn callbacks_add_registers_an_observer_that_receives_state() {
    let service = Service::start("app");
    let observer = TcpListener::bind("127.0.0.1:0").unwrap();
    let observer_addr = observer.local_addr().unwrap();
    let (received_tx, received) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = observer.accept() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).unwrap_or(0);
            let _ = received_tx.send(String::from_utf8_lossy(&request[..read]).into_owned());
        }
    });

    let url = format!("http://{observer_addr}/state");
    let added = service.simctl_json(&["callbacks", "add", "--url", &url]);
    assert_eq!(added["callbacks"], 1, "{added}");
    let request = received
        .recv_timeout(Duration::from_secs(5))
        .expect("observer receives a state POST");
    assert!(request.starts_with("POST /state"), "{request}");
}
