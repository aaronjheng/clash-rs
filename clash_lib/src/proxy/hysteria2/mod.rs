use std::{
    fmt::{Debug, Formatter},
    net::SocketAddr,
    num::ParseIntError,
    path::PathBuf,
    pin::Pin,
    str::FromStr,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};
mod codec;
mod congestion;
mod salamander;
mod udp_hop;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use h3::client::SendRequest;
use h3_quinn::OpenStreams;
use quinn::{
    crypto::rustls::QuicClientConfig, ClientConfig, Connection, TokioRuntime,
};
use quinn_proto::TransportConfig;

use rustls::{
    client::danger::{ServerCertVerified, ServerCertVerifier},
    ClientConfig as RustlsClientConfig,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Mutex,
};

use crate::{
    app::{
        dispatcher::{
            BoxedChainedDatagram, BoxedChainedStream, ChainedStreamWrapper,
        },
        dns::ThreadSafeDNSResolver,
    },
    common::utils::{encode_hex, sha256},
    // proxy::hysteria2::congestion::DynCongestion,
    session::{Session, SocksAddr},
};
use tracing::debug;

use self::{
    codec::Hy2TcpCodec,
    congestion::{Burtal, DynController},
};

use super::{
    converters::hysteria2::PortGenrateor, ConnectorType, DialWithConnector,
    OutboundHandler, OutboundType,
};

#[derive(Clone)]
pub struct HystOption {
    pub name: String,
    pub addr: SocksAddr,
    pub ports: Option<PortGenrateor>,
    pub sni: Option<String>,
    pub passwd: String,
    pub salamander: Option<String>,
    pub skip_cert_verify: bool,
    pub alpn: Vec<String>,
    pub up_down: Option<(u64, u64)>,
    pub fingerprint: Option<String>,
    pub ca: Option<PathBuf>,
    pub ca_str: Option<String>,
    pub cwnd: Option<u64>,
}

#[derive(Debug)]
struct CertVerifyOption {
    fingerprint: Option<String>,
    _ca: Option<PathBuf>,
    skip: bool,
}

impl CertVerifyOption {
    fn new(fingerprint: Option<String>, ca: Option<PathBuf>, skip: bool) -> Self {
        Self {
            fingerprint,
            _ca: ca,
            skip,
        }
    }
}

impl ServerCertVerifier for CertVerifyOption {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(ref fingerprint) = self.fingerprint {
            let cert_hex = encode_hex(&sha256(end_entity.as_ref()));
            if &cert_hex != fingerprint {
                return Err(rustls::Error::General(format!(
                    "cert hash mismatch: found: {}\nexcept: {}",
                    cert_hex, fingerprint
                )));
            }
        }

        if self.skip {
            return Ok(ServerCertVerified::assertion());
        }
        // todo
        Ok(ServerCertVerified::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![]
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
}

enum CcRx {
    Auto,
    Fixed(u64),
}

impl FromStr for CcRx {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            Ok(Self::Auto)
        } else {
            let n = s.parse::<u64>()?;
            Ok(Self::Fixed(n))
        }
    }
}

pub struct Handler {
    opts: HystOption,
    ep_config: quinn::EndpointConfig,
    client_config: quinn::ClientConfig,
    session: Mutex<Option<Arc<quinn::Connection>>>,
    // h3_conn is a copy of session, because we need h3 crate to send request, but
    // this crate have not a method to into_inner, we have to keep is
    // maybe future version of h3 crate will have a method to into_inner, or we send
    // h3 request manually, it is too complex
    h3_conn: Mutex<Option<SendRequest<OpenStreams, Bytes>>>,
    // support udp is decided by server
    support_udp: RwLock<bool>,
}

impl Debug for Handler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HystClient").finish()
    }
}

impl Handler {
    const DEFAULT_MAX_IDLE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(300);

    pub fn new(opts: HystOption) -> anyhow::Result<Self> {
        let verify = CertVerifyOption::new(
            opts.fingerprint.clone(),
            opts.ca.clone(),
            opts.skip_cert_verify,
        );
        let mut tls_config = RustlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verify))
            .with_no_client_auth();

        // should set alpn_protocol `h3` default
        tls_config.alpn_protocols = if opts.alpn.is_empty() {
            vec![b"h3".to_vec()]
        } else {
            opts.alpn.iter().map(|x| x.as_bytes().to_vec()).collect()
        };

        let mut transport = TransportConfig::default();
        // TODO
        // transport.congestion_controller_factory(DynCongestion);
        transport.max_idle_timeout(Some(
            Self::DEFAULT_MAX_IDLE_TIMEOUT.try_into().unwrap(),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));

        let quic_config: QuicClientConfig = tls_config.try_into().unwrap();
        let mut client_config = ClientConfig::new(Arc::new(quic_config));
        client_config.transport_config(Arc::new(transport));
        let ep_config = quinn::EndpointConfig::default();

        Ok(Self {
            opts: opts.clone(),
            ep_config,
            client_config,
            session: Mutex::new(None),
            h3_conn: Mutex::new(None),
            support_udp: RwLock::new(true),
        })
    }

    async fn new_authed_session(
        &self,
        resolver: ThreadSafeDNSResolver,
    ) -> anyhow::Result<(Connection, SendRequest<OpenStreams, Bytes>)> {
        // Everytime we enstablish a new session, we should lookup the server
        // address. maybe it changed since it use ddns
        let server_socket_addr = match self.opts.addr.clone() {
            SocksAddr::Ip(ip) => ip,
            SocksAddr::Domain(d, port) => {
                let ip = resolver
                    .resolve(d.as_str(), true)
                    .await?
                    .ok_or_else(|| anyhow!("resolve domain {} failed", d))?;
                SocketAddr::new(ip, port)
            }
        };

        // Here maybe we should use a AsyncUdpSocket which implement salamander obfs
        // and port hopping
        let mut ep = if self.opts.salamander.is_some() || self.opts.ports.is_some() {
            debug!("Hysteria2 use salamander obfs");

            // let udp = salamander::Salamander::new(
            //     udp_socket,
            //     self.opts.salamander.as_ref().map(|s| s.as_bytes().to_vec()),
            //     self.opts.ports.clone(),
            // )?;

            let port_gen = self.opts.ports.as_ref().unwrap().clone();
            let udp_hop =
                udp_hop::UdpHop::new(server_socket_addr.port(), port_gen, None)?;
            quinn::Endpoint::new_with_abstract_socket(
                self.ep_config.clone(),
                None,
                Arc::new(udp_hop),
                Arc::new(TokioRuntime),
            )?
        } else {
            let udp = SocketAddr::from(([0, 0, 0, 0], 0));
            // bind to port 0, so the OS will choose a random port for us
            let udp_socket = std::net::UdpSocket::bind::<SocketAddr>(udp)?;

            quinn::Endpoint::new(
                self.ep_config.clone(),
                None,
                udp_socket,
                Arc::new(TokioRuntime),
            )?
        };

        ep.set_default_client_config(self.client_config.clone());

        let session = ep
            .connect(
                server_socket_addr,
                self.opts.sni.as_ref().map(|s| s.as_str()).unwrap_or(""),
            )?
            .await?;
        let (h3_conn, _rx, udp) = Self::auth(&session, &self.opts.passwd).await?;
        *self.support_udp.write().unwrap() = udp;
        // todo set congestion controller according to cc_rx

        let any = session
            .congestion_state()
            .into_any()
            .downcast::<DynController>()
            .unwrap();
        any.set_controller(Box::new(Burtal::new(0, session.clone())));

        anyhow::Ok((session, h3_conn))
    }

    async fn auth(
        conn: &quinn::Connection,
        passwd: &str,
    ) -> anyhow::Result<(SendRequest<OpenStreams, Bytes>, CcRx, bool)> {
        let h3_conn = h3_quinn::Connection::new(conn.clone());

        let (_, mut sender) =
            h3::client::builder().build::<_, _, Bytes>(h3_conn).await?;

        let req = http::Request::post("https://hysteria/auth")
            .header("Hysteria-Auth", passwd)
            .header("Hysteria-CC-RX", "0")
            .header("Hysteria-Padding", codec::padding(64..=512))
            .body(())
            .unwrap();
        let mut r = sender.send_request(req).await?;

        let r = r.recv_response().await?;

        const HYSTERIA_STATUS_OK: u16 = 233;
        if r.status() != HYSTERIA_STATUS_OK {
            return Err(anyhow!("auth failed: response status code {}", r.status()));
        }

        // MUST have Hysteria-CC-RX and Hysteria-UDP headers according to hysteria2
        // document
        let cc_rx = r
            .headers()
            .get("Hysteria-CC-RX")
            .ok_or_else(|| anyhow!("auth failed: missing Hysteria-CC-RX header"))?
            .to_str()?
            .parse()?;

        let support_udp = r
            .headers()
            .get("Hysteria-UDP")
            .ok_or_else(|| anyhow!("auth failed: missing Hysteria-UDP header"))?
            .to_str()?
            .parse()?;

        anyhow::Ok((sender, cc_rx, support_udp))
    }
}

impl DialWithConnector for Handler {}

#[async_trait::async_trait]
impl OutboundHandler for Handler {
    fn name(&self) -> &str {
        &self.opts.name
    }

    fn proto(&self) -> OutboundType {
        OutboundType::Hysteria2
    }

    async fn support_udp(&self) -> bool {
        *self.support_udp.read().unwrap()
    }

    async fn support_connector(&self) -> ConnectorType {
        ConnectorType::Tcp
    }

    /// connect to remote target via UDP
    async fn connect_datagram(
        &self,
        _sess: &Session,
        _resolver: ThreadSafeDNSResolver,
    ) -> std::io::Result<BoxedChainedDatagram> {
        todo!()
    }

    async fn connect_stream(
        &self,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> std::io::Result<BoxedChainedStream> {
        let authed_conn = {
            let mut session_lock = self.session.lock().await;

            match (*session_lock).as_ref().filter(|s| match s.close_reason() {
                // rust should have inspect method on Option and Result!
                Some(reason) => {
                    tracing::debug!("old connection closed: {:?}", reason);
                    false
                }
                None => true,
            }) {
                Some(s) => s.clone(),
                None => {
                    let (session, h3_conn) =
                        self.new_authed_session(resolver).await.map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!(
                                    "connect to {} failed: {}",
                                    self.opts.addr, e
                                ),
                            )
                        })?;
                    let session = Arc::new(session);
                    *session_lock = Some(session.clone());
                    *self.h3_conn.lock().await = Some(h3_conn);
                    session
                }
            }
        };

        let (mut tx, mut rx) = authed_conn.open_bi().await?;

        tokio_util::codec::FramedWrite::new(&mut tx, Hy2TcpCodec)
            .send(&sess.destination)
            .await?;

        match tokio_util::codec::FramedRead::new(&mut rx, Hy2TcpCodec)
            .next()
            .await
        {
            Some(Ok(resp)) => {
                if resp.status != 0x00 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!(
                            "server response error: addr: {}, msg: {:?}",
                            self.opts.addr, resp.msg
                        ),
                    ));
                } else {
                    debug!(
                        "hysteria2 tcp request success: status: {}, msg: {:?}",
                        resp.status, resp.msg
                    );
                }
            }
            _ => {
                return Err(std::io::Error::other(format!(
                    "not receive hysteria2 response from server: {}",
                    self.opts.addr
                )));
            }
        };

        let hyster_client = HystStream { send: tx, recv: rx };
        Ok(Box::new(ChainedStreamWrapper::new(Box::new(hyster_client))))
    }
}

pub struct HystStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl Debug for HystStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HystStream").finish()
    }
}

impl AsyncRead for HystStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for HystStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().send)
            .poll_write(cx, buf)
            .map_err(|e| {
                tracing::error!("hysteria2 write error: {}", e);
                e.into()
            })
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}