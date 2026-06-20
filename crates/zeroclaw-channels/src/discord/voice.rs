//! Outbound Discord voice: speak an agent's text replies into a voice channel.
//!
//! This is the v1 "voice interaction surface" for the Discord channel - outbound
//! only. The flow is:
//!
//! 1. [`super::DiscordChannel::send`] resolves the target voice channel from
//!    config (`voice_channels[guild_id]`) and enqueues a [`VoiceRequest`].
//! 2. The [`spawn_voice_actor`] task synthesizes the text via the agent's TTS
//!    provider, transcodes it to raw 48 kHz stereo PCM with `ffmpeg`, and
//!    encodes 20 ms Opus frames with libopus (`audiopus`).
//! 3. To join a voice channel the bot must send an opcode-4 `VOICE_STATE_UPDATE`
//!    on the **main** gateway (which the actor cannot write directly), so it asks
//!    the gateway loop to do so via [`VoiceGatewayCmd`]. The gateway loop, on the
//!    resulting `VOICE_SERVER_UPDATE` / `VOICE_STATE_UPDATE` events, feeds the
//!    endpoint/token/session id back as a [`VoiceEvent`].
//! 4. With those credentials the actor opens the separate voice WebSocket + UDP
//!    socket, performs the handshake (IP discovery, encryption mode selection),
//!    and streams the Opus frames as encrypted RTP.
//!
//! Inbound voice (listening / transcription) is intentionally out of scope - it
//! belongs to the dedicated voice-host channel effort (#7943), not here.
#![cfg(feature = "channel-discord-voice")]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::voice::VoicePipeline;

// ── Audio constants (Discord voice: 48 kHz, stereo, 20 ms frames) ──
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_MS: u64 = 20;
/// PCM samples per channel in one 20 ms frame (48000 * 0.02).
const FRAME_SAMPLES_PER_CHANNEL: usize = SAMPLE_RATE as usize / 1000 * FRAME_MS as usize;
/// Interleaved i16 samples per frame (both channels).
const FRAME_SAMPLES: usize = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;
/// Bytes per frame of s16le PCM.
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// RTP timestamp increment per frame (per-channel sample count, per RFC 3550).
const RTP_TS_INCREMENT: u32 = FRAME_SAMPLES_PER_CHANNEL as u32;
/// Max encoded Opus packet size we'll buffer for one frame.
const MAX_OPUS_PACKET: usize = 1500;
/// RTP version-2 header length (no CSRCs / extensions).
const RTP_HEADER_LEN: usize = 12;
/// Poly1305 auth tag length.
const AEAD_TAG_LEN: usize = 16;
/// rtpsize trailing nonce length (a 32-bit incrementing counter).
const NONCE_TAIL_LEN: usize = 4;
/// Discord voice payload type for Opus.
const RTP_PAYLOAD_TYPE_OPUS: u8 = 0x78;
/// The encryption mode we negotiate (xsalsa20_poly1305 is deprecated).
const ENCRYPTION_MODE: &str = "aead_xchacha20_poly1305_rtpsize";

// ── Cross-task messages ──

/// A request to speak `text` into `channel_id` of `guild_id`. Enqueued by
/// `send()`; consumed by the voice actor.
pub(super) struct VoiceRequest {
    pub guild_id: String,
    pub channel_id: String,
    pub text: String,
}

/// Voice credentials the main gateway extracts from `VOICE_SERVER_UPDATE` /
/// `VOICE_STATE_UPDATE` and feeds to the voice actor.
pub(super) enum VoiceEvent {
    /// `VOICE_SERVER_UPDATE`: the voice endpoint host and connection token.
    Server {
        guild_id: String,
        endpoint: String,
        token: String,
    },
    /// `VOICE_STATE_UPDATE` for the bot's own user: the session id.
    State {
        guild_id: String,
        session_id: String,
    },
}

/// A command from the voice actor asking the main gateway loop to write an
/// opcode-4 `VOICE_STATE_UPDATE` (join a channel, or leave when `channel_id` is
/// `None`).
pub(super) struct VoiceGatewayCmd {
    pub guild_id: String,
    pub channel_id: Option<String>,
}

impl VoiceGatewayCmd {
    /// Render the opcode-4 gateway payload the main loop writes verbatim.
    pub(super) fn to_gateway_payload(&self) -> String {
        json!({
            "op": 4,
            "d": {
                "guild_id": self.guild_id,
                "channel_id": self.channel_id,
                "self_mute": false,
                "self_deaf": true,
            }
        })
        .to_string()
    }
}

/// The voice subsystem handle held by `DiscordChannel`. Created once (when the
/// channel has `voice_enabled`); the `*_rx` halves + `cmd_tx` are taken by
/// `listen()` at startup to spawn the actor, while `cmd_rx` stays in the gateway
/// loop for its opcode-4 select arm.
pub(super) struct VoiceHandle {
    /// `send()` enqueues speak requests here.
    pub request_tx: mpsc::Sender<VoiceRequest>,
    /// The gateway dispatch loop routes `VOICE_*` credentials here.
    pub event_tx: mpsc::Sender<VoiceEvent>,
    /// One-shot actor inputs (taken the first time `listen()` runs).
    bootstrap: Mutex<Option<VoiceBootstrap>>,
    /// The opcode-4 command receiver, taken + restored per gateway connection
    /// (the actor outlives any single connection).
    pub cmd_rx: Mutex<Option<mpsc::Receiver<String>>>,
}

struct VoiceBootstrap {
    request_rx: mpsc::Receiver<VoiceRequest>,
    event_rx: mpsc::Receiver<VoiceEvent>,
    cmd_tx: mpsc::Sender<String>,
}

/// The inputs `listen()` passes to [`spawn_voice_actor`].
pub(super) struct VoiceActorInputs {
    pub request_rx: mpsc::Receiver<VoiceRequest>,
    pub event_rx: mpsc::Receiver<VoiceEvent>,
    pub cmd_tx: mpsc::Sender<String>,
}

/// All voice state held by `DiscordChannel` behind a single optional field, so
/// the seam in `mod.rs` is one `#[cfg]`-gated member. Present only when the
/// channel has `voice_enabled` and a TTS-capable pipeline.
pub(super) struct VoiceState {
    pub handle: VoiceHandle,
    pub pipeline: Arc<VoicePipeline>,
    /// guild id → target voice channel id (config `voice_channels`).
    pub voice_channels: std::collections::HashMap<String, String>,
    /// text channel id → guild id, learned from inbound `MESSAGE_CREATE` so
    /// `send()` (which only knows the text channel) can resolve the guild.
    pub channel_guild: Mutex<std::collections::HashMap<String, String>>,
}

impl VoiceState {
    pub(super) fn new(
        pipeline: Arc<VoicePipeline>,
        voice_channels: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            handle: VoiceHandle::new(),
            pipeline,
            voice_channels,
            channel_guild: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve the voice channel to speak into for a reply whose text channel is
    /// `text_channel_id`: text channel → cached guild → configured VC.
    pub(super) fn resolve_target(&self, text_channel_id: &str) -> Option<(String, String)> {
        let guild = self.channel_guild.lock().get(text_channel_id).cloned()?;
        let vc = self.voice_channels.get(&guild).cloned()?;
        Some((guild, vc))
    }

    /// Remember the guild a text channel belongs to (from `MESSAGE_CREATE`).
    pub(super) fn remember_channel_guild(&self, channel_id: &str, guild_id: &str) {
        self.channel_guild
            .lock()
            .insert(channel_id.to_string(), guild_id.to_string());
    }
}

impl VoiceHandle {
    pub(super) fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        Self {
            request_tx,
            event_tx,
            bootstrap: Mutex::new(Some(VoiceBootstrap {
                request_rx,
                event_rx,
                cmd_tx,
            })),
            cmd_rx: Mutex::new(Some(cmd_rx)),
        }
    }

    /// Take the actor inputs (only the first caller gets them).
    pub(super) fn take_bootstrap(&self) -> Option<VoiceActorInputs> {
        self.bootstrap.lock().take().map(|b| VoiceActorInputs {
            request_rx: b.request_rx,
            event_rx: b.event_rx,
            cmd_tx: b.cmd_tx,
        })
    }
}

// ── The actor ──

/// Spawn the long-lived voice actor. `bot_user_id` is needed for the voice
/// IDENTIFY; `pipeline` performs TTS synthesis.
pub(super) fn spawn_voice_actor(
    mut request_rx: mpsc::Receiver<VoiceRequest>,
    mut event_rx: mpsc::Receiver<VoiceEvent>,
    cmd_tx: mpsc::Sender<String>,
    pipeline: Arc<VoicePipeline>,
    bot_user_id: Arc<String>,
) {
    zeroclaw_spawn::spawn!(async move {
        // v1 services one voice connection at a time.
        let mut active: Option<ActiveJoin> = None;
        loop {
            tokio::select! {
                req = request_rx.recv() => {
                    let Some(req) = req else { break };
                    if let Err(e) = handle_request(req, &cmd_tx, &pipeline, &mut active).await {
                        log_voice_warn("voice request failed", &e);
                    }
                }
                ev = event_rx.recv() => {
                    let Some(ev) = ev else { break };
                    apply_event(ev, &mut active);
                    if let Some(join) = active.as_mut()
                        && join.ready()
                        && let Err(e) = join.play_pending(&bot_user_id).await
                    {
                        log_voice_warn("voice playback failed", &e);
                    }
                }
            }
        }
    });
}

/// State for the one in-flight join: the target, the queued audio, and the
/// credentials as they arrive from the two `VOICE_*` events.
struct ActiveJoin {
    guild_id: String,
    channel_id: String,
    endpoint: Option<String>,
    token: Option<String>,
    session_id: Option<String>,
    /// Opus frames waiting for the connection to come up.
    pending: VecDeque<Vec<u8>>,
}

impl ActiveJoin {
    fn fresh(guild_id: String, channel_id: String) -> Self {
        Self {
            guild_id,
            channel_id,
            endpoint: None,
            token: None,
            session_id: None,
            pending: VecDeque::new(),
        }
    }

    fn ready(&self) -> bool {
        self.endpoint.is_some() && self.token.is_some() && self.session_id.is_some()
    }

    /// Connect (creds present) and stream all queued frames.
    async fn play_pending(&mut self, bot_user_id: &str) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let (Some(endpoint), Some(token), Some(session_id)) = (
            self.endpoint.clone(),
            self.token.clone(),
            self.session_id.clone(),
        ) else {
            return Ok(());
        };
        let frames: Vec<Vec<u8>> = self.pending.drain(..).collect();
        let mut conn = match VoiceConnection::connect(VoiceCreds {
            guild_id: self.guild_id.clone(),
            user_id: bot_user_id.to_string(),
            endpoint,
            token,
            session_id,
            channel_id: self.channel_id.clone(),
        })
        .await
        {
            Ok(conn) => conn,
            Err(e) => {
                // Connect failed - most often a stale endpoint/token because Discord
                // migrated the voice server (a fresh VOICE_SERVER_UPDATE is already on
                // its way). Re-stash the frames so the next `ready()` retries with the
                // refreshed creds instead of silently dropping the spoken reply.
                for frame in frames.into_iter().rev() {
                    self.pending.push_front(frame);
                }
                return Err(e);
            }
        };
        conn.play(&frames).await?;
        conn.close().await;
        Ok(())
    }
}

async fn handle_request(
    req: VoiceRequest,
    cmd_tx: &mpsc::Sender<String>,
    pipeline: &VoicePipeline,
    active: &mut Option<ActiveJoin>,
) -> Result<()> {
    if !pipeline.is_tts_available() {
        bail!("voice_enabled but no TTS provider configured for this agent");
    }
    log_voice_step(
        "voice: synthesizing reply",
        ::serde_json::json!({
            "guild_id": req.guild_id,
            "voice_channel_id": req.channel_id,
            "text_len": req.text.len(),
        }),
    );
    // Synthesize → normalize to 48 kHz stereo PCM → 20 ms Opus frames.
    let audio = pipeline
        .synthesize(&req.text)
        .await
        .context("TTS synthesis failed")?;
    let pcm = transcode_to_pcm(audio)
        .await
        .context("ffmpeg PCM transcode failed")?;
    let frames = encode_opus_frames(&pcm).context("Opus encode failed")?;
    if frames.is_empty() {
        log_voice_warn(
            "voice: TTS produced no audio frames",
            &anyhow::Error::msg(format!("guild={} - empty after encode", req.guild_id)),
        );
        return Ok(());
    }
    // Ask the gateway loop to join (opcode-4) - send the rendered payload so the
    // gateway loop can write it verbatim without depending on voice types - and
    // stash the frames to play once the voice credentials arrive.
    let join_payload = VoiceGatewayCmd {
        guild_id: req.guild_id.clone(),
        channel_id: Some(req.channel_id.clone()),
    }
    .to_gateway_payload();
    cmd_tx
        .send(join_payload)
        .await
        .map_err(|_| anyhow::Error::msg("gateway command channel closed"))?;
    log_voice_step(
        "voice: join requested (opcode-4 sent to gateway)",
        ::serde_json::json!({
            "guild_id": req.guild_id,
            "voice_channel_id": req.channel_id,
            "frames": frames.len(),
        }),
    );
    let need_reset = active
        .as_ref()
        .map(|j| j.guild_id != req.guild_id || j.channel_id != req.channel_id)
        .unwrap_or(true);
    if need_reset {
        *active = Some(ActiveJoin::fresh(req.guild_id, req.channel_id));
    }
    if let Some(join) = active.as_mut() {
        join.pending.extend(frames);
    }
    Ok(())
}

fn apply_event(ev: VoiceEvent, active: &mut Option<ActiveJoin>) {
    let Some(join) = active.as_mut() else { return };
    match ev {
        VoiceEvent::Server {
            guild_id,
            endpoint,
            token,
        } if guild_id == join.guild_id => {
            log_voice_step(
                "voice: gateway voice-server creds applied",
                ::serde_json::json!({ "guild_id": guild_id.as_str(), "endpoint": endpoint.as_str() }),
            );
            join.endpoint = Some(endpoint);
            join.token = Some(token);
        }
        VoiceEvent::State {
            guild_id,
            session_id,
        } if guild_id == join.guild_id => {
            log_voice_step(
                "voice: voice-state session id applied",
                ::serde_json::json!({ "guild_id": guild_id.as_str(), "session_present": !session_id.is_empty() }),
            );
            join.session_id = Some(session_id);
        }
        _ => {}
    }
}

// ── Audio: TTS bytes → 48 kHz stereo PCM → Opus frames ──

/// Transcode arbitrary audio bytes to raw s16le / 48 kHz / stereo PCM via an
/// `ffmpeg` subprocess (the same dependency the TTS manager already requires).
async fn transcode_to_pcm(audio: Vec<u8>) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::Command;

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-f",
            "s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(
            "failed to spawn ffmpeg - ensure ffmpeg with libopus support is installed \
             (e.g. `sudo apt install ffmpeg`)",
        )?;

    let mut stdin = child.stdin.take().context("ffmpeg stdin unavailable")?;
    let write = async move {
        stdin.write_all(&audio).await?;
        stdin.shutdown().await?;
        Ok::<(), std::io::Error>(())
    };
    let (write_result, output) = tokio::join!(write, child.wait_with_output());
    write_result.context("failed to write audio to ffmpeg stdin")?;
    let output = output.context("ffmpeg process error")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg transcode to PCM failed: {stderr}");
    }
    if output.stdout.is_empty() {
        bail!("ffmpeg produced empty PCM output");
    }
    Ok(output.stdout)
}

/// Encode 48 kHz stereo s16le PCM into a vector of 20 ms Opus packets. A trailing
/// partial frame is zero-padded to a full frame.
fn encode_opus_frames(pcm: &[u8]) -> Result<Vec<Vec<u8>>> {
    use audiopus::coder::Encoder;
    use audiopus::{Application, Channels, SampleRate};

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .map_err(|e| anyhow::Error::msg(format!("opus encoder init failed: {e}")))?;

    let mut frames = Vec::with_capacity(pcm.len() / FRAME_BYTES + 1);
    let mut out = vec![0u8; MAX_OPUS_PACKET];
    for chunk in pcm.chunks(FRAME_BYTES) {
        // Convert interleaved s16le bytes → i16, zero-padding the last frame.
        let mut samples = [0i16; FRAME_SAMPLES];
        for (i, pair) in chunk.chunks_exact(2).enumerate() {
            samples[i] = i16::from_le_bytes([pair[0], pair[1]]);
        }
        let written = encoder
            .encode(&samples, &mut out)
            .map_err(|e| anyhow::Error::msg(format!("opus encode failed: {e}")))?;
        frames.push(out[..written].to_vec());
    }
    Ok(frames)
}

// ── RTP packetizer (pure; unit-tested) ──

/// Builds `aead_xchacha20_poly1305_rtpsize` packets and owns the RTP sequence /
/// timestamp / nonce counters. Separated from the socket/WS so it is testable in
/// isolation.
struct RtpPacketizer {
    ssrc: u32,
    secret_key: [u8; 32],
    sequence: u16,
    timestamp: u32,
    nonce: u32,
}

impl RtpPacketizer {
    fn new(ssrc: u32, secret_key: [u8; 32]) -> Self {
        Self {
            ssrc,
            secret_key,
            sequence: 0,
            timestamp: 0,
            nonce: 0,
        }
    }

    /// Build one encrypted RTP packet: a plaintext 12-byte RTP header
    /// (authenticated as AAD), the encrypted Opus payload + tag, then the 4-byte
    /// incrementing nonce as the packet footer.
    fn packet(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        use chacha20poly1305::aead::AeadInPlace as _;
        use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};

        let mut header = [0u8; RTP_HEADER_LEN];
        header[0] = 0x80; // version 2, no padding/extension/CSRC
        header[1] = RTP_PAYLOAD_TYPE_OPUS;
        header[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        header[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        self.nonce = self.nonce.wrapping_add(1);
        let nonce_tail = self.nonce.to_be_bytes();
        let mut xnonce = [0u8; 24];
        xnonce[..NONCE_TAIL_LEN].copy_from_slice(&nonce_tail);

        let cipher = XChaCha20Poly1305::new((&self.secret_key).into());
        let mut buffer = opus.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(XNonce::from_slice(&xnonce), &header, &mut buffer)
            .map_err(|e| anyhow::Error::msg(format!("voice packet encryption failed: {e}")))?;

        let mut packet =
            Vec::with_capacity(RTP_HEADER_LEN + buffer.len() + AEAD_TAG_LEN + NONCE_TAIL_LEN);
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&buffer);
        packet.extend_from_slice(tag.as_slice());
        packet.extend_from_slice(&nonce_tail);
        Ok(packet)
    }

    /// Advance the RTP counters by one 20 ms frame.
    fn advance(&mut self) {
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(RTP_TS_INCREMENT);
    }
}

// ── Voice connection: WS handshake + UDP + encrypted RTP ──

struct VoiceCreds {
    guild_id: String,
    user_id: String,
    endpoint: String,
    token: String,
    session_id: String,
    /// The target voice channel id (the DAVE/MLS group id) for E2EE.
    channel_id: String,
}

/// A live voice connection: the UDP socket, voice WS, RTP packetizer, and the
/// DAVE end-to-end-encryption session (`Some` once the gateway negotiates a
/// non-zero DAVE protocol version in SESSION_DESCRIPTION).
struct VoiceConnection {
    udp: UdpSocket,
    ws: WsStream,
    rtp: RtpPacketizer,
    dave: Option<davey::DaveSession>,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl VoiceConnection {
    async fn connect(creds: VoiceCreds) -> Result<Self> {
        // The endpoint arrives without a scheme; voice gateway v8 over wss.
        let host = creds.endpoint.trim_end_matches(":80");
        let url = format!("wss://{host}/?v=8");
        log_voice_step(
            "voice: connecting to voice gateway",
            ::serde_json::json!({ "guild_id": creds.guild_id.as_str(), "endpoint": host }),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .with_context(|| format!("voice gateway connect failed ({url})"))?;

        // op 0 IDENTIFY. We advertise DAVE support (`max_dave_protocol_version`):
        // Discord made end-to-end encryption MANDATORY for voice on 2026-03-01 and
        // removed the unencrypted fallback, so a non-DAVE client is rejected with
        // close 4017. We join the MLS group and E2E-encrypt each Opus frame (the
        // crypto is handled by the `davey` crate).
        let dave_user_id: u64 = creds.user_id.parse().unwrap_or(0);
        let dave_channel_id: u64 = creds.channel_id.parse().unwrap_or(0);
        let identify = json!({
            "op": 0,
            "d": {
                "server_id": creds.guild_id,
                "user_id": creds.user_id,
                "session_id": creds.session_id,
                "token": creds.token,
                "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
            }
        });
        ws.send(Message::Text(identify.to_string().into())).await?;

        // Drive the handshake until READY (op 2), SESSION_DESCRIPTION (op 4), and
        // when the gateway negotiates a non-zero DAVE version, the MLS group join
        // (the DAVE session reports `is_ready`) have all completed.
        let mut ssrc = 0u32;
        let mut secret_key = [0u8; 32];
        let mut have_ready = false;
        let mut have_session = false;
        let mut udp: Option<UdpSocket> = None;
        // DAVE state.
        let mut dave: Option<davey::DaveSession> = None;
        let mut dave_version: u16 = 0;
        // The member ids announced via CLIENTS_CONNECT (op 11), passed to
        // `process_proposals` so we refuse adds for users not in the call.
        let mut recognized_user_ids: Vec<u64> = Vec::new();
        // transition_id -> protocol_version, applied on EXECUTE_TRANSITION (op 22).
        let mut pending_transitions: std::collections::HashMap<u64, u16> =
            std::collections::HashMap::new();

        let dave_done = |dave: &Option<davey::DaveSession>, ver: u16| {
            ver == 0 || dave.as_ref().is_some_and(|d| d.is_ready())
        };
        while !(have_ready && have_session && dave_done(&dave, dave_version)) {
            let msg = tokio::time::timeout(Duration::from_secs(15), ws.next())
                .await
                .context("voice handshake timed out")?
                .context("voice gateway closed during handshake (stream ended)")??;
            let text = match msg {
                Message::Text(text) => text,
                // DAVE MLS opcodes arrive as BINARY frames: [u16 seq BE][u8 op][payload].
                // The `davey` session owns all the MLS crypto; we just route bytes.
                Message::Binary(buf) => {
                    if buf.len() < 3 {
                        continue;
                    }
                    let op = buf[2];
                    let data = &buf[3..];
                    let Some(session) = dave.as_mut() else {
                        continue;
                    };
                    match op {
                        // op 25 MLS_EXTERNAL_SENDER: the gateway's signing key.
                        25 => session.set_external_sender(data).map_err(|e| {
                            anyhow::Error::msg(format!("DAVE external sender failed: {e}"))
                        })?,
                        // op 27 MLS_PROPOSALS: [optype:1][proposals]. May yield a
                        // commit (+ optional welcome) we must broadcast as op 28.
                        27 => {
                            let optype = if data.first().copied().unwrap_or(0) == 0 {
                                davey::ProposalsOperationType::APPEND
                            } else {
                                davey::ProposalsOperationType::REVOKE
                            };
                            if let Some(cw) = session
                                .process_proposals(optype, &data[1..], Some(&recognized_user_ids))
                                .map_err(|e| {
                                    anyhow::Error::msg(format!("DAVE proposals failed: {e}"))
                                })?
                            {
                                let mut out = cw.commit;
                                if let Some(welcome) = cw.welcome {
                                    out.extend_from_slice(&welcome);
                                }
                                ws.send(dave_binary(28, &out)).await?;
                            }
                        }
                        // op 29 MLS_ANNOUNCE_COMMIT_TRANSITION: [tid:2][commit].
                        29 if data.len() >= 2 => {
                            let tid = u16::from_be_bytes([data[0], data[1]]) as u64;
                            session.process_commit(&data[2..]).map_err(|e| {
                                anyhow::Error::msg(format!("DAVE commit failed: {e}"))
                            })?;
                            if tid != 0 {
                                pending_transitions.insert(tid, dave_version);
                                ws.send(Message::Text(
                                    json!({ "op": 23, "d": { "transition_id": tid } })
                                        .to_string()
                                        .into(),
                                ))
                                .await?;
                            }
                            log_voice_step(
                                "voice: DAVE commit processed",
                                ::serde_json::json!({ "transition_id": tid, "ready": session.is_ready() }),
                            );
                        }
                        // op 30 MLS_WELCOME: [tid:2][welcome]. This joins us to the group.
                        30 if data.len() >= 2 => {
                            let tid = u16::from_be_bytes([data[0], data[1]]) as u64;
                            session.process_welcome(&data[2..]).map_err(|e| {
                                anyhow::Error::msg(format!("DAVE welcome failed: {e}"))
                            })?;
                            if tid != 0 {
                                pending_transitions.insert(tid, dave_version);
                                ws.send(Message::Text(
                                    json!({ "op": 23, "d": { "transition_id": tid } })
                                        .to_string()
                                        .into(),
                                ))
                                .await?;
                            }
                            log_voice_step(
                                "voice: DAVE welcome processed: MLS group joined",
                                ::serde_json::json!({ "transition_id": tid, "ready": session.is_ready() }),
                            );
                        }
                        _ => {}
                    }
                    continue;
                }
                // A CLOSE frame carries Discord's reason code - surface it so a
                // silent-no-audio failure is diagnosable rather than a bare
                // "stream ended" (4004 auth failed, 4006 session no longer valid
                // after a voice-server migration, 4016 bad encryption mode, ...).
                Message::Close(frame) => {
                    let (code, reason) = frame
                        .map(|f| (u16::from(f.code), f.reason.to_string()))
                        .unwrap_or((0, String::new()));
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(
                                ::serde_json::json!({ "close_code": code, "reason": reason })
                            ),
                        "voice: gateway sent CLOSE during handshake"
                    );
                    bail!("voice gateway closed during handshake (close code {code})");
                }
                // Ping/Pong/Binary/Frame are not part of the JSON op handshake.
                _ => continue,
            };
            let payload: Value = serde_json::from_str(&text)?;
            match payload.get("op").and_then(Value::as_u64) {
                Some(2) => {
                    let d = payload.get("d").context("voice READY missing d")?;
                    ssrc = d
                        .get("ssrc")
                        .and_then(Value::as_u64)
                        .context("READY ssrc")? as u32;
                    let server_ip = d
                        .get("ip")
                        .and_then(Value::as_str)
                        .context("READY ip")?
                        .to_string();
                    let server_port = d
                        .get("port")
                        .and_then(Value::as_u64)
                        .context("READY port")? as u16;
                    let advertised = advertised_modes(d);
                    if !advertised.iter().any(|m| m == ENCRYPTION_MODE) {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "required": ENCRYPTION_MODE,
                                "advertised": advertised,
                            })),
                            "voice: Discord did not offer the required encryption mode"
                        );
                        bail!("voice server does not offer {ENCRYPTION_MODE}");
                    }
                    log_voice_step(
                        "voice: READY - encryption mode selected",
                        ::serde_json::json!({
                            "ssrc": ssrc,
                            "server_ip": server_ip,
                            "server_port": server_port,
                            "mode": ENCRYPTION_MODE,
                            "advertised": advertised,
                        }),
                    );
                    let sock = UdpSocket::bind("0.0.0.0:0").await?;
                    sock.connect((server_ip.as_str(), server_port)).await?;
                    let (ext_ip, ext_port) = ip_discovery(&sock, ssrc).await?;
                    log_voice_step(
                        "voice: UDP IP discovery complete",
                        ::serde_json::json!({ "external_ip": ext_ip.as_str(), "external_port": ext_port }),
                    );
                    // op 1 SELECT_PROTOCOL
                    let select = json!({
                        "op": 1,
                        "d": {
                            "protocol": "udp",
                            "data": { "address": ext_ip, "port": ext_port, "mode": ENCRYPTION_MODE }
                        }
                    });
                    ws.send(Message::Text(select.to_string().into())).await?;
                    udp = Some(sock);
                    have_ready = true;
                }
                Some(4) => {
                    let d = payload.get("d").context("SESSION_DESCRIPTION missing d")?;
                    let key = d
                        .get("secret_key")
                        .and_then(Value::as_array)
                        .context("SESSION_DESCRIPTION secret_key")?;
                    if key.len() != 32 {
                        bail!("unexpected secret_key length {}", key.len());
                    }
                    for (i, b) in key.iter().enumerate() {
                        secret_key[i] = b.as_u64().unwrap_or(0) as u8;
                    }
                    log_voice_step(
                        "voice: SESSION_DESCRIPTION - secret_key received",
                        ::serde_json::json!({ "key_len": key.len() }),
                    );
                    have_session = true;
                    // DAVE: the gateway announces the negotiated E2EE protocol
                    // version. v>0 -> create the MLS session and send our key package
                    // so the gateway can add us to the group.
                    dave_version = d
                        .get("dave_protocol_version")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u16;
                    if dave_version > 0 {
                        let ver = std::num::NonZeroU16::new(dave_version)
                            .context("non-zero dave version")?;
                        let mut session =
                            davey::DaveSession::new(ver, dave_user_id, dave_channel_id, None)
                                .map_err(|e| {
                                    anyhow::Error::msg(format!("DAVE session init failed: {e}"))
                                })?;
                        let key_package = session.create_key_package().map_err(|e| {
                            anyhow::Error::msg(format!("DAVE key package failed: {e}"))
                        })?;
                        ws.send(dave_binary(26, &key_package)).await?;
                        dave = Some(session);
                        log_voice_step(
                            "voice: DAVE active: MLS key package sent",
                            ::serde_json::json!({ "dave_version": dave_version }),
                        );
                    }
                }
                // op 11 CLIENTS_CONNECT: the member ids in the call. Track them so
                // process_proposals refuses adds for users not actually present.
                Some(11) => {
                    if let Some(ids) = payload
                        .get("d")
                        .and_then(|d| d.get("user_ids"))
                        .and_then(Value::as_array)
                    {
                        for id in ids
                            .iter()
                            .filter_map(|v| v.as_str())
                            .filter_map(|s| s.parse::<u64>().ok())
                        {
                            if !recognized_user_ids.contains(&id) {
                                recognized_user_ids.push(id);
                            }
                        }
                    }
                }
                // op 13 CLIENT_DISCONNECT: drop a member id.
                Some(13) => {
                    if let Some(id) = payload
                        .get("d")
                        .and_then(|d| d.get("user_id"))
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        recognized_user_ids.retain(|&u| u != id);
                    }
                }
                // op 21 DAVE_PREPARE_TRANSITION: a transition to `protocol_version` is
                // pending. tid 0 = immediate (re)init; otherwise ack via op 23.
                Some(21) => {
                    let d = payload.get("d");
                    let tid = d
                        .and_then(|d| d.get("transition_id"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let pv = d
                        .and_then(|d| d.get("protocol_version"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u16;
                    pending_transitions.insert(tid, pv);
                    if tid == 0 {
                        dave_version = pv;
                    } else {
                        ws.send(Message::Text(
                            json!({ "op": 23, "d": { "transition_id": tid } })
                                .to_string()
                                .into(),
                        ))
                        .await?;
                    }
                    log_voice_step(
                        "voice: DAVE prepare-transition",
                        ::serde_json::json!({ "transition_id": tid, "protocol_version": pv }),
                    );
                }
                // op 22 DAVE_EXECUTE_TRANSITION: apply the pending version.
                Some(22) => {
                    let tid = payload
                        .get("d")
                        .and_then(|d| d.get("transition_id"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if let Some(pv) = pending_transitions.remove(&tid) {
                        dave_version = pv;
                    }
                    log_voice_step(
                        "voice: DAVE execute-transition",
                        ::serde_json::json!({ "transition_id": tid, "dave_version": dave_version }),
                    );
                }
                // op 24 DAVE_PREPARE_EPOCH: epoch 1 = a brand-new MLS group; reinit.
                Some(24) => {
                    let d = payload.get("d");
                    let epoch = d
                        .and_then(|d| d.get("epoch"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let pv = d
                        .and_then(|d| d.get("protocol_version"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u16;
                    if epoch == 1
                        && let Some(ver) = std::num::NonZeroU16::new(pv)
                        && let Ok(mut session) =
                            davey::DaveSession::new(ver, dave_user_id, dave_channel_id, None)
                        && let Ok(kp) = session.create_key_package()
                    {
                        dave_version = pv;
                        ws.send(dave_binary(26, &kp)).await?;
                        dave = Some(session);
                    }
                }
                _ => {}
            }
        }

        let udp = udp.context("voice UDP socket never established")?;
        log_voice_step(
            "voice: voice connection established",
            ::serde_json::json!({
                "ssrc": ssrc,
                "dave_version": dave_version,
                "e2ee": dave.as_ref().is_some_and(|d| d.is_ready()),
            }),
        );
        Ok(Self {
            udp,
            ws,
            rtp: RtpPacketizer::new(ssrc, secret_key),
            dave,
        })
    }

    /// Announce transmission (op 5 SPEAKING), send each Opus frame as encrypted
    /// RTP paced at 20 ms, then flush with five silence frames.
    async fn play(&mut self, frames: &[Vec<u8>]) -> Result<()> {
        let speaking = json!({
            "op": 5,
            "d": { "speaking": 1, "delay": 0, "ssrc": self.rtp.ssrc }
        });
        self.ws
            .send(Message::Text(speaking.to_string().into()))
            .await?;
        log_voice_step(
            "voice: speaking - streaming RTP frames",
            ::serde_json::json!({ "ssrc": self.rtp.ssrc, "frames": frames.len() }),
        );

        let mut ticker = tokio::time::interval(Duration::from_millis(FRAME_MS));
        for frame in frames {
            ticker.tick().await;
            // DAVE end-to-end encryption (inner layer): when the MLS group is
            // ready, E2E-encrypt the Opus payload before the transport RTP layer
            // wraps + encrypts the packet. Without a ready DAVE session we send the
            // plain Opus (only happens on non-DAVE servers, now effectively none).
            let e2ee: std::borrow::Cow<'_, [u8]> = match self.dave.as_mut() {
                Some(d) if d.is_ready() => d
                    .encrypt_opus(frame)
                    .map_err(|e| anyhow::Error::msg(format!("DAVE encrypt failed: {e}")))?,
                _ => std::borrow::Cow::Borrowed(frame.as_slice()),
            };
            let packet = self.rtp.packet(e2ee.as_ref())?;
            self.udp.send(&packet).await?;
            self.rtp.advance();
        }

        // Five frames of Opus silence flush the decoder (Discord recommendation).
        let silence: [u8; 3] = [0xF8, 0xFF, 0xFE];
        for _ in 0..5 {
            ticker.tick().await;
            let packet = self.rtp.packet(&silence)?;
            self.udp.send(&packet).await?;
            self.rtp.advance();
        }
        log_voice_step(
            "voice: playback complete",
            ::serde_json::json!({ "frames_sent": frames.len(), "silence_frames": 5 }),
        );
        Ok(())
    }

    async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// UDP IP discovery (Discord voice): send a 74-byte request, parse our external
/// address from the response.
async fn ip_discovery(sock: &UdpSocket, ssrc: u32) -> Result<(String, u16)> {
    let mut req = [0u8; 74];
    req[0..2].copy_from_slice(&1u16.to_be_bytes()); // type = request
    req[2..4].copy_from_slice(&70u16.to_be_bytes()); // length
    req[4..8].copy_from_slice(&ssrc.to_be_bytes());
    sock.send(&req).await?;

    let mut resp = [0u8; 74];
    let n = tokio::time::timeout(Duration::from_secs(5), sock.recv(&mut resp))
        .await
        .context("IP discovery timed out")??;
    if n < 74 {
        bail!("short IP discovery response ({n} bytes)");
    }
    // Address is a null-terminated string in bytes 8..72; port is the last 2 BE.
    let end = resp[8..72].iter().position(|&b| b == 0).unwrap_or(64);
    let ip = String::from_utf8_lossy(&resp[8..8 + end]).to_string();
    let port = u16::from_be_bytes([resp[72], resp[73]]);
    Ok((ip, port))
}

/// The encryption modes Discord advertised in the voice READY (op 2) payload.
fn advertised_modes(ready_d: &Value) -> Vec<String> {
    ready_d
        .get("modes")
        .and_then(Value::as_array)
        .map(|modes| {
            modes
                .iter()
                .filter_map(|m| m.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Frame an outgoing DAVE binary opcode. Unlike inbound frames (which carry a
/// `[u16 seq][u8 op]` header), the client sends `[u8 op][payload]`.
fn dave_binary(op: u8, data: &[u8]) -> Message {
    let mut buf = Vec::with_capacity(1 + data.len());
    buf.push(op);
    buf.extend_from_slice(data);
    Message::Binary(buf.into())
}

fn log_voice_warn(msg: &str, err: &anyhow::Error) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "error": err.to_string() })),
        msg
    );
}

/// INFO milestone in the outbound voice flow. One per handshake stage lets a
/// live test watch progress - the last line logged before silence pinpoints
/// exactly which stage stalled (join, creds, WS handshake, encryption, play).
fn log_voice_step(msg: &str, attrs: ::serde_json::Value) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(attrs),
        msg
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_constants_are_20ms_48k_stereo() {
        assert_eq!(FRAME_SAMPLES_PER_CHANNEL, 960);
        assert_eq!(FRAME_SAMPLES, 1920);
        assert_eq!(FRAME_BYTES, 3840);
        assert_eq!(RTP_TS_INCREMENT, 960);
    }

    #[test]
    fn rtp_packet_layout_and_nonce_increment() {
        let mut rtp = RtpPacketizer::new(0xDEAD_BEEF, [9u8; 32]);
        rtp.sequence = 7;
        rtp.timestamp = 960;
        let opus = vec![0xAAu8; 40];
        let pkt = rtp.packet(&opus).unwrap();

        // header(12) + ciphertext(40) + tag(16) + nonce(4)
        assert_eq!(pkt.len(), 12 + 40 + 16 + 4);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], RTP_PAYLOAD_TYPE_OPUS);
        assert_eq!(&pkt[2..4], &7u16.to_be_bytes());
        assert_eq!(&pkt[4..8], &960u32.to_be_bytes());
        assert_eq!(&pkt[8..12], &0xDEAD_BEEFu32.to_be_bytes());
        // nonce footer = the (post-increment) counter, big-endian.
        assert_eq!(&pkt[pkt.len() - 4..], &1u32.to_be_bytes());
        assert_eq!(rtp.nonce, 1);
        // Ciphertext must differ from plaintext (it actually encrypted).
        assert_ne!(&pkt[12..52], opus.as_slice());
    }

    #[test]
    fn rtp_packet_roundtrip_decrypts_with_header_aad() {
        use chacha20poly1305::aead::AeadInPlace as _;
        use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};

        let mut rtp = RtpPacketizer::new(1, [3u8; 32]);
        let opus = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let pkt = rtp.packet(&opus).unwrap();

        let header = &pkt[..RTP_HEADER_LEN];
        let ct_len = pkt.len() - RTP_HEADER_LEN - AEAD_TAG_LEN - NONCE_TAIL_LEN;
        let mut buf = pkt[RTP_HEADER_LEN..RTP_HEADER_LEN + ct_len].to_vec();
        let tag = &pkt[RTP_HEADER_LEN + ct_len..RTP_HEADER_LEN + ct_len + AEAD_TAG_LEN];
        let nonce_tail = &pkt[pkt.len() - NONCE_TAIL_LEN..];
        let mut xnonce = [0u8; 24];
        xnonce[..NONCE_TAIL_LEN].copy_from_slice(nonce_tail);

        let cipher = XChaCha20Poly1305::new((&[3u8; 32]).into());
        cipher
            .decrypt_in_place_detached(XNonce::from_slice(&xnonce), header, &mut buf, tag.into())
            .expect("decrypt with header AAD must succeed");
        assert_eq!(buf, opus);
    }

    #[test]
    fn advance_steps_sequence_and_timestamp() {
        let mut rtp = RtpPacketizer::new(1, [0u8; 32]);
        rtp.advance();
        assert_eq!(rtp.sequence, 1);
        assert_eq!(rtp.timestamp, RTP_TS_INCREMENT);
    }

    #[test]
    fn gateway_cmd_payload_join_and_leave() {
        let join = VoiceGatewayCmd {
            guild_id: "g1".into(),
            channel_id: Some("vc1".into()),
        };
        let v: Value = serde_json::from_str(&join.to_gateway_payload()).unwrap();
        assert_eq!(v["op"], 4);
        assert_eq!(v["d"]["guild_id"], "g1");
        assert_eq!(v["d"]["channel_id"], "vc1");
        assert_eq!(v["d"]["self_deaf"], true);

        let leave = VoiceGatewayCmd {
            guild_id: "g1".into(),
            channel_id: None,
        };
        let v: Value = serde_json::from_str(&leave.to_gateway_payload()).unwrap();
        assert!(v["d"]["channel_id"].is_null());
    }
}
