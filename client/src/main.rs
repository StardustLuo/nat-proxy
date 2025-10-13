mod config;

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    os::fd::AsRawFd,
};

use bincode::{Decode, config::standard};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    select,
    sync::watch,
    time::{Duration, sleep},
};
use tracing::{debug, error, info, level_filters::LevelFilter};
use tracing_subscriber::EnvFilter;

#[derive(Decode)]
struct ConnectRequest {
    ip: [u8; 4],
    port: u16,
}

fn to_ipv6_mapped(ipv4: Ipv4Addr) -> Ipv6Addr {
    let [a, b, c, d] = ipv4.octets();
    Ipv6Addr::new(
        0,
        0,
        0,
        0,
        0,
        0xfefe,
        ((a as u16) << 8) + b as u16,
        ((c as u16) << 8) + d as u16,
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::init("config.toml");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_thread_names(true)
        .with_line_number(true)
        .with_file(true)
        .init();

    let service_addr = SocketAddr::V4(SocketAddrV4::new(
        cfg.server_ip.parse().unwrap(),
        cfg.service_port,
    ));
    let bridge_addr = SocketAddr::V4(SocketAddrV4::new(
        cfg.server_ip.parse().unwrap(),
        cfg.bridge_port,
    ));
    let local_service_addr = format!("[::1]:{}", cfg.local_service_port).parse::<SocketAddr>()?;
    loop {
        let service = match TcpStream::connect(service_addr).await {
            Err(e) => {
                error!("service connect err: {:?}. retry after 10s...", e);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
            Ok(conn) => conn,
        };
        info!("connected to remote server {}", service_addr);
        let (mut service_rx, mut service_wx) = service.into_split();
        let (abort_tx, mut abort_rx) = watch::channel(false);
        const HEARTBEAT: [u8; 8] = [0xff, 0xff, 0xfe, 0xfe, 0xef, 0xef, 0xee, 0xee];
        // heartbeat sender
        {
            let abort_tx = abort_tx.clone();
            tokio::spawn(async move {
                loop {
                    select! {
                        _tick = sleep(Duration::from_secs(10)) => {
                            if let Err(e) = service_wx.write_all(&HEARTBEAT).await {
                                error!("send heartbeat err: {:?}", e);
                                let _ = abort_tx.send(true);
                                break;
                            };
                        }
                        _abort = abort_rx.changed() => {
                            if *abort_rx.borrow() == true {
                                break;
                            }
                        }
                    }
                }
            });
        }
        let mut buf = [0u8; 8];
        loop {
            // waiting for bridge request from proxy-server
            select! {
                recv = service_rx.read_exact(&mut buf) => {
                    match recv {
                        Ok(n) => {
                            if n == 8 && buf == HEARTBEAT {
                                debug!("heartbeat received");
                                continue;
                            }
                        },
                        Err(e) if e.kind() == tokio::io::ErrorKind::UnexpectedEof => {
                            info!("service closed normally");
                            break;
                        },
                        Err(e) => {
                            error!("service err: {:?}", e);
                            break;
                        },
                    };

                    // parse origin ipv4 address
                    let (req, _): (ConnectRequest, usize) = bincode::decode_from_slice(&buf, standard()).unwrap();
                    let (client_ipv4, client_port) = (std::net::Ipv4Addr::from(req.ip), req.port);
                    info!("client connection from {}:{}", client_ipv4, client_port);

                    // initiate new bridge connection
                    let mut bridge = match TcpStream::connect(bridge_addr).await {
                        Err(e) => {
                            error!("initiate bridge connection err: {:?}", e);
                            break;
                        },
                        Ok(bridge) => {
                            debug!("bridge connection inititated");
                            bridge
                        },
                    };

                    // map to ipv6 address and connect to local service
                    let client_ipv6_mapped = to_ipv6_mapped(client_ipv4);
                    let client_addr_mapped = SocketAddr::V6(SocketAddrV6::new(client_ipv6_mapped, client_port, 0, 0));
                    let local_service_socket = TcpSocket::new_v6()?;
                    if let Err(e) = local_service_socket.bind(client_addr_mapped) {
                        error!("local_service_socket bind err: {}, addr: {}", e, client_addr_mapped);
                        continue;
                    }
                    let mut local_service = match local_service_socket.connect(local_service_addr).await {
                        Err(e) => {
                            error!("local service connect err: {:?}", e);
                            continue;
                        }
                        Ok(service) => service,
                    };
                    // bridge <-> local_service
                    tokio::spawn(async move {
                        let fd = bridge.as_raw_fd();
                        debug!("start bridging for {}:{}, fd: {}", client_ipv4, client_port, fd);
                        let res = tokio::io::copy_bidirectional(&mut bridge, &mut local_service).await;
                        debug!("end bridging for {}:{}, fd: {}, res: {:?}", client_ipv4, client_port, fd, res);
                    });
                }
                // heartbeat timeout
                _tick = tokio::time::sleep(Duration::from_secs(20)) => {
                    error!("heartbeat timeout");
                    break;
                }
            }
        }
        let _ = abort_tx.send(true);
    }
}
