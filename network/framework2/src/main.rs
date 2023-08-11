// Copyright © Aptos Foundation

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::str::FromStr;
use std::io::{Result,Error};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use aptos_types::network_address::{parse_ip_tcp,NetworkAddress};
use aptos_network2::util::invalid_addr_error;

fn main() {
    let rt = Builder::new_multi_thread().enable_all().thread_name("netmain").build().unwrap();
    let _rte = rt.enter();
    let result = rt.block_on(result_main());
    match result {
        Err(e) => {
            println!("err: {}", e);
        }
        Ok(_) => {
            println!("Ok!");
        }
    }
}

async fn result_main() -> Result<()> {
    let na = NetworkAddress::from_str("/ip4/127.0.0.1/tcp/8301").map_err(|e| Error::new(ErrorKind::InvalidInput, e.to_string()))?;
    let mut listener = Listener::new(na, ConnectionType::Validator).await?;
    println!("Hello, world!");
    listener.listen_loop().await;
    return Ok(());
}

pub struct Network {
    peers: Vec<Peer>,

    listeners: TcpListener,
}

struct Listener {
    socket: TcpListener,
    policy: ConnectionType,
}

impl Listener {
    pub async fn new(addr: NetworkAddress, policy: ConnectionType) -> std::io::Result<Self> {
        let ((ipaddr, port), addr_suffix) =
            parse_ip_tcp(addr.as_slice()).ok_or_else(|| invalid_addr_error(&addr))?;
        if !addr_suffix.is_empty() {
            return Err(invalid_addr_error(&addr));
        }

        let addr = SocketAddr::new(ipaddr, port);

        let socket = if ipaddr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };

        // TODO: bring back configurable buffer sizes
        // if let Some(rx_buf) = self.tcp_buff_cfg.inbound_rx_buffer_bytes {
        //     socket.set_recv_buffer_size(rx_buf)?;
        // }
        // if let Some(tx_buf) = self.tcp_buff_cfg.inbound_tx_buffer_bytes {
        //     socket.set_send_buffer_size(tx_buf)?;
        // }
        socket.set_reuseaddr(true)?;
        socket.bind(addr)?;

        let socket = socket.listen(256)?;

        Ok(Listener{
            socket,
            policy,
        })
    }

    pub async fn listen_loop(&mut self) -> Result<()> {
        loop {
            match self.socket.accept().await {
                Ok((socket, addr)) => {
                    self.start_inbound_peer(socket,addr).await;
                },
                Err(e) => {}, // TODO: log; or return Err if bad enough
            }
        }
        Ok(())
    }

    pub async fn start_inbound_peer(&mut self, stream: tokio::net::TcpStream, addr: SocketAddr) {

    }
}

pub struct Peer {
    socket: tokio::net::TcpStream,
    policy: ConnectionType,
}

pub enum ConnectionType {
    Unknown,
    Validator,
    VFN,
    PFN,
    Other,
}
