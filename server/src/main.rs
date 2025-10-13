mod config;

use anyhow::Result;
use bincode::{Encode, config::standard};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    select,
    sync::{Mutex, watch},
    time::{sleep, timeout},
};
use tracing::{debug, error, info, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Encode)]
struct ConnectRequest {
    ip: [u8; 4],
    port: u16,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
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

    loop {
        // waiting for service connection
        let (service, service_addr) = {
            let service_listener =
                match TcpListener::bind(format!("0.0.0.0:{}", cfg.service_port)).await {
                    Ok(res) => res,
                    Err(e) => {
                        error!("service_listener bind err: {:?}", e);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                };
            info!("listening for service connection...");
            match service_listener.accept().await {
                Err(e) => {
                    error!("service_listener err: {:?}", e);
                    continue;
                }
                Ok(conn) => {
                    info!("service connection from {}", conn.1);
                    conn
                }
            }
        };
        let (mut service_rx, service_wx) = service.into_split();
        let service_wx = Arc::new(Mutex::new(service_wx));
        let (abort_tx, mut abort_rx) = watch::channel(false);

        const HEARTBEAT: [u8; 8] = [0xff, 0xff, 0xfe, 0xfe, 0xef, 0xef, 0xee, 0xee];

        // heartbeat sender
        {
            let service_wx = service_wx.clone();
            let (abort_tx, mut abort_rx) = (abort_tx.clone(), abort_rx.clone());
            tokio::spawn(async move {
                loop {
                    select! {
                        _tick = sleep(Duration::from_secs(10)) => {
                            if let Err(e) = service_wx.lock().await.write_all(&HEARTBEAT).await {
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

        // heartbeat listener
        {
            let abort_tx = abort_tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8];
                loop {
                    select! {
                        biased;
                        recv = service_rx.read_exact(&mut buf) => {
                            match recv {
                                Ok(n) => {
                                    if n == 8 && buf == HEARTBEAT {
                                        continue;
                                    }
                                    error!("service connection received unexpected message");
                                },
                                Err(e) if e.kind() == tokio::io::ErrorKind::UnexpectedEof => info!("service connection closed normally"),
                                Err(e) => error!("service connection err: {:?}", e),
                            }
                            break;
                        }
                        // heartbeat timeout
                        _tick = tokio::time::sleep(Duration::from_secs(20)) => {
                            error!("heartbeat timeout");
                            break;
                        }
                        // TODO: add watch?
                    }
                }
                let _ = abort_tx.send(true);
            });
        }

        let client_listener = match TcpListener::bind(format!("0.0.0.0:{}", cfg.client_port)).await
        {
            Ok(res) => res,
            Err(e) => {
                error!("client_listener bind err: {:?}", e);
                continue;
            }
        };

        loop {
            debug!("listening for client connection...");
            select! {
                client = client_listener.accept() => {
                    match client {
                        Err(e) => {
                            error!("client_listener err: {:?}", e);
                            let _ = abort_tx.send(true);
                            break;
                        },
                        Ok((mut client, client_addr)) => {
                            let (client_ip, client_port) = match client_addr {
                                std::net::SocketAddr::V4(v4) => (v4.ip().octets(), v4.port()),
                                _ => continue,
                            };
                            info!("client connection from {}:{}", std::net::Ipv4Addr::from(client_ip), client_port);

                            // request new bridge connections
                            let bridge_listener = match TcpListener::bind(format!("0.0.0.0:{}", cfg.bridge_port)).await {
                                Err(e) => {
                                    error!("bridge_listener bind err: {:?}", e);
                                    continue;
                                },
                                Ok(listener) => listener,
                            };
                            let req = ConnectRequest { ip: client_ip, port: client_port };
                            let mut buf = [0u8; 8];
                            bincode::encode_into_slice(&req, &mut buf, standard()).unwrap();
                            if let Err(e) = service_wx.lock().await.write_all(&buf).await {
                                error!("service err: {:?}", e);
                                let _ = abort_tx.send(true);
                                break;
                            };

                            // waiting for bridge connection from proxy-client
                            match timeout(Duration::from_secs(10), bridge_listener.accept()).await {
                                Ok(Ok((mut bridge, bridge_addr))) => {
                                    // make sure that bridge connection comes from the same ip of service connection
                                    if bridge_addr.ip() == service_addr.ip() {
                                        // client <-> bridge
                                        tokio::spawn(async move {
                                            let _ = tokio::io::copy_bidirectional(&mut client, &mut bridge).await;
                                        });
                                    }
                                },
                                Ok(Err(e)) => {
                                    error!("bridge_listener err: {:?}", e);
                                },
                                Err(_) => {
                                    warn!("bridge time limit exceeded");
                                },
                            }
                        },
                    }
                }
                _abort = abort_rx.changed() => {
                    if *abort_rx.borrow() == true {
                        break;
                    }
                }
            }
        }
    }
}
