use crate::app::nat_manager::UdpPacket;
use crate::config::internal::config::BindAddress;
use crate::proxy::{http, AnyInboundListener, InboundListener};
use crate::session::{Network, Session, SocksAddr};

use crate::proxy::utils::Interface;
use crate::{Dispatcher, Error, NatManager, Runner};
use futures::FutureExt;
use log::info;
use network_interface::{Addr, NetworkInterfaceConfig};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{Receiver, Sender};

pub enum ListenerType {
    HTTP,
}

pub struct NetworkInboundListener {
    pub name: String,
    pub bind_addr: BindAddress,
    pub port: u16,
    pub listener_type: ListenerType,
    pub dispatcher: Arc<Dispatcher>,
    pub nat_manager: Arc<NatManager>,
}

impl NetworkInboundListener {
    pub fn listen(&self) -> Result<Vec<Runner>, Error> {
        let mut runners = Vec::<Runner>::new();

        match &self.bind_addr {
            BindAddress::Any => {
                #[cfg(target_os = "ios")]
                {
                    let all_ifaces =
                        network_interface::NetworkInterface::show().expect("list interfaces");

                    for iface in all_ifaces.into_iter() {
                        let ip =
                            iface
                                .addr
                                .map(|x| x.ip())
                                .filter(|x| x.is_ipv4())
                                .map(|x| match x {
                                    IpAddr::V4(v4) => v4,
                                    IpAddr::V6(_) => unreachable!(),
                                });

                        if !ip.is_some() {
                            continue;
                        }

                        let ip = ip.unwrap();
                        if ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() {
                            continue;
                        }

                        self.build_and_insert_listener(&mut runners, ip);
                    }
                }
                #[cfg(not(target_os = "ios"))]
                {
                    let ip = "0.0.0.0".parse().expect("must parse");
                    self.build_and_insert_listener(&mut runners, ip);
                }
            }
            BindAddress::One(iface) => match iface {
                Interface::IpAddr(ip) => match ip {
                    IpAddr::V4(ip) => self.build_and_insert_listener(&mut runners, ip.clone()),
                    IpAddr::V6(_) => unreachable!("unsupported listening v6"),
                },
                Interface::Name(iface) => {
                    let ip = network_interface::NetworkInterface::show()
                        .expect("list interfaces")
                        .into_iter()
                        .filter(|x| &x.name == iface)
                        .map(|x| x.addr)
                        .flatten()
                        .map(|x| match x {
                            Addr::V4(v4) => v4.ip,
                            Addr::V6(_) => unreachable!(),
                        })
                        .find(|x| !x.is_unspecified() && !x.is_link_local() && !x.is_multicast())
                        .expect("no valid ip");

                    self.build_and_insert_listener(&mut runners, ip);
                }
            },
        };

        Ok(runners)
    }

    fn build_and_insert_listener(&self, runners: &mut Vec<Runner>, ip: Ipv4Addr) {
        let listener = match self.listener_type {
            ListenerType::HTTP => {
                http::Listener::new((ip, self.port).into(), self.dispatcher.clone())
            }
        };

        if listener.handle_tcp() {
            info!("{} TCP listening at: {}:{}", self.name, ip, self.port);

            let tcp_listener = listener.clone();
            runners.push(
                async move {
                    if let Err(e) = tcp_listener.listen_tcp().await {
                        log::warn!("handler tcp listen failed: {}", e);
                    }
                }
                .boxed(),
            );
        }

        if listener.handle_udp() {
            info!("{} UDP listening at: {}:{}", self.name, ip, self.port);
            let udp_listener = listener.clone();
            runners.push(
                async move {
                    if let Err(e) = udp_listener.listen_udp().await {
                        log::warn!("handler udp listen failed: {}", e);
                    }
                }
                .boxed(),
            );
        }
    }
}