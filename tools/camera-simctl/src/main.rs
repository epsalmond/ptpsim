//! `camera-simctl` — CLI over the simulator's control HTTP plus a black-box PTP
//! smoke client (design gate #5). Std-only and synchronous: it is a thin
//! operator/test tool, not a service.

use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use protocol_primitives::fuji_framing;
use ptp_core::{DeviceInfo, InitCommandRequest, OperationRequest, PtpCodec, PtpIpPacket, Reader};

#[derive(Parser)]
#[command(
    name = "camera-simctl",
    about = "Control + smoke-test a ptpsim instance"
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
    /// POST /shutdown on the control endpoint.
    Shutdown {
        #[arg(long, default_value = "127.0.0.1:8080")]
        control: String,
    },
    /// Black-box PTP/IP smoke: init, open session, device info, enumerate, and
    /// download the first object. Exits non-zero on any failure.
    Smoke {
        /// PTP command socket address.
        #[arg(long, default_value = "127.0.0.1:55740")]
        host: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Health { control } => {
            println!("{}", http(&control, "GET", "/healthz")?);
        }
        Cmd::Shutdown { control } => {
            println!("{}", http(&control, "POST", "/shutdown")?);
        }
        Cmd::Smoke { host } => smoke(&host)?,
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

fn write_frame(s: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    s.write_all(bytes)?;
    Ok(())
}

fn read_frame(s: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    buf[0..4].copy_from_slice(&len);
    s.read_exact(&mut buf[4..])?;
    Ok(buf)
}

fn op(code: u16, tid: u32, params: Vec<u32>) -> Vec<u8> {
    fuji_framing::encode(&PtpIpPacket::OperationRequest(OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id: tid,
        params,
    }))
    .unwrap()
}

fn read_data_reply(s: &mut TcpStream) -> Result<Vec<u8>> {
    let _start = fuji_framing::decode(&read_frame(s)?)?;
    let end = fuji_framing::decode(&read_frame(s)?)?;
    let resp = fuji_framing::decode(&read_frame(s)?)?;
    let data = match end {
        PtpIpPacket::EndData(d) => d.payload,
        other => bail!("expected EndData, got {other:?}"),
    };
    match resp {
        PtpIpPacket::OperationResponse(r) if r.code == 0x2001 => {}
        other => bail!("expected OK response, got {other:?}"),
    }
    Ok(data)
}

fn read_ok(s: &mut TcpStream) -> Result<()> {
    match fuji_framing::decode(&read_frame(s)?)? {
        PtpIpPacket::OperationResponse(r) if r.code == 0x2001 => Ok(()),
        other => bail!("expected OK, got {other:?}"),
    }
}

fn smoke(host: &str) -> Result<()> {
    let mut s = TcpStream::connect(host).with_context(|| format!("connect {host}"))?;
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [9; 16],
        friendly_name: "camera-simctl".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init)?)?;
    match PtpIpPacket::decode(&read_frame(&mut s)?)? {
        PtpIpPacket::InitCommandAck(a) => println!("init ok: camera says '{}'", a.friendly_name),
        other => bail!("expected InitCommandAck, got {other:?}"),
    }

    write_frame(&mut s, &op(0x1002, 1, vec![1]))?;
    read_ok(&mut s)?;
    println!("session open");

    write_frame(&mut s, &op(0x1001, 2, vec![]))?;
    let di = DeviceInfo::decode(&read_data_reply(&mut s)?)?;
    println!(
        "device: {} {} ({} ops)",
        di.manufacturer,
        di.model,
        di.operations_supported.len()
    );

    write_frame(&mut s, &op(0x1007, 3, vec![0x00010001, 0, 0]))?;
    let handles_bytes = read_data_reply(&mut s)?;
    let mut r = Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32())?;
    println!("{} object(s) on card", handles.len());
    if handles.is_empty() {
        println!("no media fixtures present — nothing to download");
        return Ok(());
    }

    write_frame(&mut s, &op(0x101b, 4, vec![handles[0], 0, 64]))?;
    let part = read_data_reply(&mut s)?;
    println!(
        "downloaded {} bytes of object {:#010x}",
        part.len(),
        handles[0]
    );

    write_frame(&mut s, &op(0x1003, 5, vec![]))?;
    read_ok(&mut s)?;
    println!("session closed — smoke OK");
    Ok(())
}
