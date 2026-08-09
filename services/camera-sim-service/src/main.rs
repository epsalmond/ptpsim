//! Entry point: parse config, bind, install a SIGTERM handler, and serve until

use std::net::SocketAddr;
use std::path::PathBuf;

use camera_sim::StateOverlay;
use camera_sim_service::{Config, Server};
use clap::Parser;

mod logging;

#[derive(Parser)]
#[command(name = "camera-sim-service", about = "ptpsim camera simulator service")]
struct Args {
    /// Profile id (label only; e.g. fuji/gfx100ii). The rich manifest is passed via
    /// --manifest (e.g. .../gfx100ii.consolidated.yaml); fw deltas are overlays.
    #[arg(long, default_value = "fuji/gfx100ii")]
    profile: String,

    #[arg(long, default_value = "local")]
    instance_id: String,
    /// Manifest connection persona to serve.
    #[arg(long, default_value = "app")]
    connection: String,
    /// Path to the camera manifest YAML.
    #[arg(long)]
    manifest: PathBuf,
    /// Optional startup-state YAML/JSON overlay applied before any listener
    /// serves traffic.
    #[arg(long)]
    startup_state: Option<PathBuf>,
    /// Media card root (contains DCIM/...).
    #[arg(long)]
    media_root: PathBuf,
    /// PTP command socket bind. Unset uses the selected manifest connection port.
    #[arg(long)]
    command_bind: Option<SocketAddr>,
    /// Async event socket bind. Unset omits the socket when the selected
    /// connection has no event role.
    #[arg(long)]
    event_bind: Option<SocketAddr>,
    /// Live-view stream socket bind. Unset omits the socket when the selected
    /// connection has no live-view role.
    #[arg(long)]
    liveview_bind: Option<SocketAddr>,
    /// Optional PCSS UDP knock listener bind for LAN-fidelity wireless tethering.
    #[arg(long)]
    knock_bind: Option<SocketAddr>,
    /// Number of PCSS InitFail packets to emit before InitCommandAck.
    #[arg(long, default_value_t = 0)]
    pcss_init_fails: u32,
    /// PCSS handles to enqueue after each shutter sequence; 0 seeds all at startup.
    #[arg(long, default_value_t = 0)]
    pcss_shutter_enqueue_count: u32,
    /// Control HTTP bind (loopback by default).
    #[arg(long, default_value = "127.0.0.1:8080")]
    control_bind: SocketAddr,
    /// Directory of JPEG frames the live-view socket emits, looped in sorted-
    /// filename order at ~30 fps. Unset / empty dir => the socket accepts but
    /// emits no frames. Frames are gated on engine Phase::Streaming (after
    /// InitiateOpenCapture), matching a real camera.
    #[arg(long)]
    liveview_dir: Option<PathBuf>,
    /// Optional observer URL. When set, the service POSTs a JSON snapshot of
    /// camera state (phase, session, device-property map, media object count) to
    /// this URL whenever it changes — fire-and-forget, debounced, never blocks
    /// the responder. e.g. the client application dev panel: http://127.0.0.1:8770/state
    #[arg(long)]
    state_callback: Option<String>,
    /// Log format. `auto` uses compact human logs on a terminal and JSON
    /// otherwise.
    #[arg(long, value_enum, default_value_t = logging::LogFormat::Auto)]
    log_format: logging::LogFormat,
    /// Color policy for compact logs. `auto` respects terminal detection and
    /// NO_COLOR.
    #[arg(long, value_enum, default_value_t = logging::ColorChoice::Auto)]
    color: logging::ColorChoice,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    logging::init(args.log_format, args.color);
    let manifest_yaml = std::fs::read_to_string(&args.manifest)?;
    let startup_state = match args.startup_state.as_deref() {
        Some(path) => Some(load_startup_state(path)?),
        None => None,
    };
    let config = Config {
        instance_id: args.instance_id,
        profile: args.profile,
        connection: args.connection,
        manifest_yaml,
        media_root: args.media_root,
        command_bind: args.command_bind,
        liveview_bind: args.liveview_bind,
        event_bind: args.event_bind,
        knock_bind: args.knock_bind,
        pcss_init_fails: args.pcss_init_fails,
        pcss_shutter_enqueue_count: args.pcss_shutter_enqueue_count,
        control_bind: args.control_bind,
        liveview_dir: args.liveview_dir,
        state_callback: args.state_callback,
    };

    let server = Server::bind(config).await?;
    if let Some(overlay) = startup_state.as_ref() {
        let applied = server
            .apply_startup_state(overlay)
            .await
            .map_err(|e| anyhow::anyhow!("startup state rejected: {e}"))?;
        tracing::info!(
            props = applied.props,
            phase = applied.phase,
            session_open = applied.session_open,
            camera_initiated_transfer_active = applied.camera_initiated_transfer_active,
            "startup state applied"
        );
    }
    tracing::info!(command = %server.command_addr(), control = %server.control_addr(), "ptpsim listening");

    let (tx, rx) = tokio::sync::oneshot::channel();
    // SIGTERM / Ctrl-C -> graceful shutdown.
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tracing::info!("shutdown signal received");
        let _ = tx.send(());
    });

    server.run(rx).await;
    tracing::info!("stopped");
    Ok(())
}

fn load_startup_state(path: &std::path::Path) -> anyhow::Result<StateOverlay> {
    let raw = std::fs::read_to_string(path)?;
    serde_yaml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse startup state {}: {e}", path.display()))
}
