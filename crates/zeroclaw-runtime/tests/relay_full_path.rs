//! Full real-component path: zerocode-style client -> relay -> daemon bridge ->
//! daemon WSS listener, with the inner mTLS completing end to end.
//!
//! Unlike the relay crate's make-or-break (which uses an inline bridge and a raw
//! mTLS echo), this drives the REAL [`zeroclaw_runtime::relay::run_relay_bridge`]
//! against a WSS (TLS + WebSocket) mTLS listener built from the runtime's own
//! `build_tls_acceptor` - i.e. the daemon's actual remote-plane stack - and a
//! client that speaks WebSocket-over-TLS through the relay.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use zeroclaw_relay_proto::Frame;

#[derive(Debug)]
struct NoServerVerify;
impl rustls::client::danger::ServerCertVerifier for NoServerVerify {
    fn verify_server_cert(
        &self,
        _e: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _n: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _t: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn write_temp(content: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

async fn write_frame(sock: &mut TcpStream, frame: &Frame) {
    sock.write_all(frame.to_line().as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
}

async fn read_frame(sock: &mut TcpStream) -> Frame {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = sock.read(&mut byte).await.unwrap();
        assert!(n == 1, "closed before control frame");
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    Frame::from_line(&String::from_utf8(buf).unwrap()).unwrap()
}

#[tokio::test]
async fn zerocode_to_relay_to_daemon_full_path() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Server (daemon) materials + an issued client cert.
    let dir = tempfile::tempdir().unwrap();
    let mats = zeroclaw_tls::ensure_server_materials(dir.path(), &[]).unwrap();
    let acceptor: TlsAcceptor = zeroclaw_runtime::rpc::wss::build_tls_acceptor(
        mats.server_cert_path.to_str().unwrap(),
        mats.server_key_path.to_str().unwrap(),
        mats.ca_cert_path.to_str().unwrap(),
        &[],
    )
    .unwrap();

    let ca_pem = std::fs::read_to_string(&mats.ca_cert_path).unwrap();
    let ca_key_pem = std::fs::read_to_string(&mats.ca_key_path).unwrap();
    let issued = zeroclaw_tls::issue_client_cert(&ca_pem, &ca_key_pem, "relay-device").unwrap();
    let cert_f = write_temp(&issued.cert_pem);
    let key_f = write_temp(&issued.key_pem);
    let client_chain = zeroclaw_tls::load_certs(cert_f.path().to_str().unwrap()).unwrap();
    let client_key = zeroclaw_tls::load_private_key(key_f.path().to_str().unwrap()).unwrap();

    // Daemon WSS listener (TLS + WebSocket echo), exactly the remote-plane stack.
    let wss = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wss_addr = wss.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = wss.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                    return;
                };
                if let Some(Ok(msg)) = ws.next().await {
                    let _ = ws.send(msg).await; // echo
                    let _ = ws.flush().await;
                }
            });
        }
    });

    // Relay.
    let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay_listener.local_addr().unwrap();
    tokio::spawn(
        zerorelay::RelayServer::new(zerorelay::RelayConfig::default()).serve(relay_listener),
    );

    // The REAL daemon-side bridge.
    let cancel = CancellationToken::new();
    tokio::spawn(zeroclaw_runtime::relay::run_relay_bridge(
        relay_addr.to_string(),
        "relay-device".to_string(),
        String::new(),
        format!("127.0.0.1:{}", wss_addr.port()),
        cancel.clone(),
    ));

    // Client: relay handshake, then WebSocket-over-mTLS through the relay. Retry
    // until the (asynchronously spawned) bridge has registered the node-id.
    let mut sock = None;
    for _ in 0..100 {
        let mut s = TcpStream::connect(relay_addr).await.unwrap();
        write_frame(
            &mut s,
            &Frame::Connect {
                node_id: "relay-device".into(),
            },
        )
        .await;
        match read_frame(&mut s).await {
            Frame::Opened { .. } => {
                sock = Some(s);
                break;
            }
            Frame::Error { .. } => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            other => panic!("unexpected relay reply: {other:?}"),
        }
    }
    let sock = sock.expect("bridge did not register the node-id in time");

    let client_cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(NoServerVerify))
    .with_client_auth_cert(client_chain, client_key)
    .unwrap();
    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(client_cfg));

    let (mut ws, _resp) = tokio_tungstenite::client_async_tls_with_config(
        "wss://relay-device/",
        sock,
        None,
        Some(connector),
    )
    .await
    .expect("WebSocket-over-mTLS must complete through the relay");

    ws.send(Message::Text("ping".into())).await.unwrap();
    let echoed = ws.next().await.expect("echo").expect("ws message");
    assert_eq!(
        echoed.into_text().unwrap(),
        "ping",
        "echo did not round-trip via relay"
    );

    cancel.cancel();
}
