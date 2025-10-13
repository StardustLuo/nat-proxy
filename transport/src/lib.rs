//! Configurable TCP/TLS transport for control and bridge connections.

use std::{fs::File, io::BufReader, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        self, RootCertStore,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
    },
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Async byte stream shared by plain TCP and TLS transports.
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

/// Owned stream usable with Tokio split and bidirectional copying.
pub type Stream = Box<dyn IoStream>;

/// Client TLS settings, loaded once before entering the connection loop.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientTlsConfig {
    pub enabled: bool,
    pub ca_file: Option<String>,
    pub server_name: Option<String>,
}

/// Server TLS settings, loaded once before accepting connections.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerTlsConfig {
    pub enabled: bool,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
}

/// Reusable connector selecting TCP or verified TLS according to configuration.
pub struct Connector {
    tls: Option<(TlsConnector, ServerName<'static>)>,
}

impl Connector {
    /// Load trusted PEM certificates and the expected server identity when TLS is enabled.
    pub fn new(config: &ClientTlsConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self { tls: None });
        }
        let path = config
            .ca_file
            .as_deref()
            .context("tls.ca_file is required")?;
        let mut roots = RootCertStore::empty();
        for cert in certificates(Path::new(path))? {
            roots.add(cert).context("invalid trusted certificate")?;
        }
        let name = config
            .server_name
            .clone()
            .context("tls.server_name is required")?;
        let name = ServerName::try_from(name).context("invalid tls.server_name")?;
        let tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            tls: Some((TlsConnector::from(Arc::new(tls)), name)),
        })
    }

    /// Connect via TCP and, if configured, complete a verified TLS handshake within ten seconds.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Stream> {
        let tcp = TcpStream::connect(addr).await?;
        match &self.tls {
            None => Ok(Box::new(tcp)),
            Some((connector, name)) => {
                let stream = timeout(HANDSHAKE_TIMEOUT, connector.connect(name.clone(), tcp))
                    .await
                    .context("TLS handshake timed out")?
                    .context("TLS client handshake failed")?;
                Ok(Box::new(stream))
            }
        }
    }
}

/// Reusable server transport accepting either TCP or TLS connections.
pub struct Acceptor {
    tls: Option<TlsAcceptor>,
}

impl Acceptor {
    /// Load the PEM certificate chain and private key when TLS is enabled.
    pub fn new(config: &ServerTlsConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self { tls: None });
        }
        let cert_path = config
            .cert_file
            .as_deref()
            .context("tls.cert_file is required")?;
        let key_path = config
            .key_file
            .as_deref()
            .context("tls.key_file is required")?;
        let certs = certificates(Path::new(cert_path))?;
        let key = PrivateKeyDer::from_pem_file(key_path)
            .with_context(|| format!("cannot load TLS private key: {key_path}"))?;
        let tls = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("invalid TLS certificate/key pair")?;
        Ok(Self {
            tls: Some(TlsAcceptor::from(Arc::new(tls))),
        })
    }

    /// Wrap an accepted TCP stream, completing the configured TLS handshake within ten seconds.
    pub async fn accept(&self, tcp: TcpStream) -> Result<Stream> {
        match &self.tls {
            None => Ok(Box::new(tcp)),
            Some(acceptor) => {
                let stream = timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
                    .await
                    .context("TLS handshake timed out")?
                    .context("TLS server handshake failed")?;
                Ok(Box::new(stream))
            }
        }
    }
}

/// Read a nonempty certificate chain or trust bundle from a PEM file.
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let certs = CertificateDer::pem_reader_iter(BufReader::new(file))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("cannot parse certificates in {}", path.display()))?;
    ensure!(!certs.is_empty(), "no certificates in {}", path.display());
    Ok(certs)
}
