//! Entry point: parse config, bind, install a SIGTERM handler, and serve until


use std::net::SocketAddr;
use std::path::PathBuf;

use camera_sim_service::{Config, Server};
use clap::Parser;

#[derive(Parser)]
#[command(name = "camera-sim-service", about = "ptpsim camera simulator service")]
struct Args {
    /// Profile id (label only; e.g. fuji/gfx100ii). The rich manifest is passed via
    /// --manifest (e.g. .../gfx100ii.consolidated.yaml); fw deltas are overlays.
    #[arg(long, default_value = "fuji/gfx100ii")]
    profile: String,

    #[arg(long, default_value = "local")]
    instance_id: String,
    /// Path to the camera manifest YAML.
    #[arg(long)]
    manifest: PathBuf,
    /// Media card root (contains DCIM/...).
    #[arg(long)]
    media_root: PathBuf,
    /// PTP command socket bind. Default binds all IPv6 (Apple review is IPv6-only).
    #[arg(long, default_value = "[::]:55740")]
    command_bind: SocketAddr,
    /// Async event socket (command+1 per the shipping app).
    #[arg(long, default_value = "[::]:55741")]
    event_bind: SocketAddr,
    /// Live-view (through-picture) stream socket (command+2 per the shipping app).
    #[arg(long, default_value = "[::]:55742")]
    liveview_bind: SocketAddr,
    /// Control HTTP bind (loopback by default).
    #[arg(long, default_value = "127.0.0.1:8080")]
    control_bind: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let args = Args::parse();
    let manifest_yaml = std::fs::read_to_string(&args.manifest)?;
    let config = Config {
        instance_id: args.instance_id,
        profile: args.profile,
        manifest_yaml,
        media_root: args.media_root,
        command_bind: args.command_bind,
        liveview_bind: args.liveview_bind,
        event_bind: args.event_bind,
        control_bind: args.control_bind,
    };

    let server = Server::bind(config).await?;
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
