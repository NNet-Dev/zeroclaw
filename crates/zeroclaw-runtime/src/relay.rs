//! Daemon-side relay bridge (runtime-owned).
//!
//! Keeps a persistent *control* connection to a nominated relay, registers the
//! daemon's `node_id`, and on each `Open` opens a *data* connection to the relay
//! and pipes it to the local WSS RPC listener on loopback. The inner
//! client<->daemon mTLS terminates at the WSS listener exactly as on the direct
//! path; this bridge and the relay only move ciphertext.
//!
//! Reconnects with capped exponential backoff; cancellation stops it promptly.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use zeroclaw_relay_proto::Frame;

const MAX_FRAME: usize = 64 * 1024;
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A session that stayed up at least this long is treated as "established", so
/// its disconnect resets the backoff (transient drop) rather than escalating it.
const ESTABLISHED: Duration = Duration::from_secs(5);

/// Run the relay bridge until `cancel` fires, reconnecting with backoff.
///
/// `relay_addr` is the relay's `host:port`; `local_wss_addr` is the loopback
/// address of the daemon's own WSS listener (e.g. `127.0.0.1:9781`).
pub async fn run_relay_bridge(
    relay_addr: String,
    node_id: String,
    relay_token: String,
    local_wss_addr: String,
    cancel: CancellationToken,
) -> Result<()> {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let started = Instant::now();
        match serve_once(
            &relay_addr,
            &node_id,
            &relay_token,
            &local_wss_addr,
            &cancel,
        )
        .await
        {
            Ok(()) => return Ok(()), // clean cancellation
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "relay": relay_addr,
                            "node_id": node_id,
                            "error": format!("{e:#}"),
                        })),
                    "relay bridge connection lost; will retry"
                );
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        if started.elapsed() >= ESTABLISHED {
            backoff = BACKOFF_INITIAL;
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
    }
}

async fn serve_once(
    relay_addr: &str,
    node_id: &str,
    relay_token: &str,
    local_wss_addr: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut ctrl = TcpStream::connect(relay_addr)
        .await
        .with_context(|| format!("connecting to relay {relay_addr}"))?;
    write_frame(
        &mut ctrl,
        &Frame::Register {
            node_id: node_id.to_string(),
            relay_token: relay_token.to_string(),
        },
    )
    .await?;
    match read_frame(&mut ctrl).await? {
        Frame::Registered { .. } => {}
        Frame::Error { code, msg } => anyhow::bail!("relay rejected registration: {code}: {msg}"),
        other => anyhow::bail!("unexpected relay reply to register: {other:?}"),
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            frame = read_frame(&mut ctrl) => {
                // Only `Open` is expected on the daemon link; ignore anything else.
                if let Frame::Open { conn_id } = frame? {
                    let relay_addr = relay_addr.to_string();
                    let local = local_wss_addr.to_string();
                    zeroclaw_spawn::spawn!(async move {
                        let _ = bridge_conn(&relay_addr, conn_id, &local).await;
                    });
                }
            }
        }
    }
}

/// Pipe one client connection (identified by `conn_id`) between the relay and
/// the local WSS listener. Both sides are opaque bytes (the inner mTLS).
async fn bridge_conn(relay_addr: &str, conn_id: u64, local_wss_addr: &str) -> Result<()> {
    let mut data = TcpStream::connect(relay_addr)
        .await
        .context("opening relay data connection")?;
    write_frame(
        &mut data,
        &Frame::Accept {
            conn_id,
            relay_token: String::new(),
        },
    )
    .await?;
    let mut local = TcpStream::connect(local_wss_addr)
        .await
        .with_context(|| format!("connecting local WSS listener {local_wss_addr}"))?;
    tokio::io::copy_bidirectional(&mut data, &mut local)
        .await
        .context("relay<->local pipe")?;
    Ok(())
}

async fn write_frame(sock: &mut TcpStream, frame: &Frame) -> Result<()> {
    sock.write_all(frame.to_line().as_bytes())
        .await
        .context("writing relay frame")?;
    sock.flush().await.ok();
    Ok(())
}

/// Read one newline-terminated control frame without over-reading into the byte
/// stream that follows.
async fn read_frame(sock: &mut TcpStream) -> Result<Frame> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = sock.read(&mut byte).await.context("reading relay frame")?;
        if n == 0 {
            anyhow::bail!("relay closed the connection");
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > MAX_FRAME {
            anyhow::bail!("relay control frame exceeds {MAX_FRAME} bytes");
        }
    }
    let line = String::from_utf8(buf).context("relay frame is not UTF-8")?;
    Frame::from_line(&line).context("parsing relay frame")
}
