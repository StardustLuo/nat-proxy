//! Real loopback connections exercise transport selection and certificate verification.

use proxy_transport::{Acceptor, ClientTlsConfig, Connector, ServerTlsConfig};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

/// Generate an isolated server certificate and matching transport settings.
fn tls_configs() -> (tempfile::TempDir, ClientTlsConfig, ServerTlsConfig) {
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.path().join("server.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
    let client = ClientTlsConfig {
        enabled: true,
        ca_file: Some(cert_path.to_str().unwrap().into()),
        server_name: Some("localhost".into()),
    };
    let server = ServerTlsConfig {
        enabled: true,
        cert_file: Some(cert_path.to_str().unwrap().into()),
        key_file: Some(key_path.to_str().unwrap().into()),
    };
    (dir, client, server)
}

/// Exchange bytes through split streams and the same copying primitive used by the proxy.
async fn roundtrip(client: ClientTlsConfig, server: ServerTlsConfig) {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = Acceptor::new(&server).unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            let (mut local, mut echo) = tokio::io::duplex(64);
            let echo_task = tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(&mut echo);
                tokio::io::copy(&mut reader, &mut writer).await.unwrap();
                writer.shutdown().await.unwrap();
            });
            tokio::io::copy_bidirectional(&mut stream, &mut local)
                .await
                .unwrap();
            echo_task.await.unwrap();
        });
        let stream = Connector::new(&client)
            .unwrap()
            .connect(addr)
            .await
            .unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        for message in [b"heartbeat".as_slice(), b"bridge payload".as_slice()] {
            writer.write_all(message).await.unwrap();
            writer.flush().await.unwrap();
            let mut reply = vec![0; message.len()];
            reader.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, message);
        }
        writer.shutdown().await.unwrap();
        let mut remaining = Vec::new();
        reader.read_to_end(&mut remaining).await.unwrap();
        task.await.unwrap();
    })
    .await
    .expect("roundtrip timed out");
}

/// Existing configurations select plain TCP and support byte forwarding.
#[tokio::test]
async fn default_tcp_roundtrip() {
    let client = toml::from_str::<ClientTlsConfig>("").unwrap();
    let server = toml::from_str::<ServerTlsConfig>("").unwrap();
    assert!(!client.enabled && !server.enabled);
    roundtrip(client, server).await;
}

/// Trusted TLS supports control messages, forwarding, and graceful shutdown.
#[tokio::test]
async fn trusted_tls_roundtrip() {
    let (_dir, client, server) = tls_configs();
    roundtrip(client, server).await;
}

/// Connect to a TLS listener and ensure invalid server credentials are rejected.
async fn rejected(client: ClientTlsConfig, server: ServerTlsConfig) {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let acceptor = Acceptor::new(&server).unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            assert!(acceptor.accept(tcp).await.is_err());
        });
        assert!(
            Connector::new(&client)
                .unwrap()
                .connect(addr)
                .await
                .is_err()
        );
        task.await.unwrap();
    })
    .await
    .expect("certificate rejection timed out");
}

/// A trusted certificate must also match the configured server name.
#[tokio::test]
async fn wrong_server_name_is_rejected() {
    let (_dir, mut client, server) = tls_configs();
    client.server_name = Some("wrong.example".into());
    rejected(client, server).await;
}

/// Certificates outside the configured trust bundle are rejected.
#[tokio::test]
async fn untrusted_certificate_is_rejected() {
    let (_dir, _, server) = tls_configs();
    let (_other_dir, client, _) = tls_configs();
    rejected(client, server).await;
}

/// Enabling TLS requires complete settings before any connection is attempted.
#[test]
fn missing_tls_settings_are_rejected() {
    assert!(
        Connector::new(&ClientTlsConfig {
            enabled: true,
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        Acceptor::new(&ServerTlsConfig {
            enabled: true,
            ..Default::default()
        })
        .is_err()
    );
}
