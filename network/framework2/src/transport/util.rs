// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use tokio::runtime::Handle;
use aptos_config::config::NetworkConfig;
use aptos_config::network_id::{NetworkContext, PeerNetworkId};
use aptos_logger::{info, warn};
#[cfg(any(test, feature = "testing", feature = "fuzzing"))]
use aptos_netcore::transport::memory::MemoryTransport;
use aptos_netcore::transport::tcp::TcpTransport;
use aptos_netcore::transport::Transport;
use aptos_types::network_address::NetworkAddress;
use crate::application::ApplicationCollector;
use crate::application::storage::PeersAndMetadata;
use crate::peer;
use crate::protocols::network::OutboundPeerConnections;
use crate::transport::AptosNetTransport;
use futures::AsyncWriteExt;

#[derive(Clone)]
pub enum AptosNetTransportActual {
    Tcp(AptosNetTransport<TcpTransport>),
    #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
    Memory(AptosNetTransport<MemoryTransport>),
}

impl AptosNetTransportActual {
    pub async fn dial(
        &mut self,
        remote_peer_network_id: PeerNetworkId,
        network_address: NetworkAddress,
        config: &NetworkConfig,
        apps: Arc<ApplicationCollector>,
        handle: Handle,
        peers_and_metadata: Arc<PeersAndMetadata>,
        peer_senders: Arc<OutboundPeerConnections>,
        network_context: NetworkContext,
    ) -> io::Result<()> {
        match self {
            AptosNetTransportActual::Tcp(tt) => {
                connect_outbound(tt, remote_peer_network_id, network_address, config, apps, handle.clone(), peers_and_metadata, peer_senders, network_context).await
            }
            #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
            AptosNetTransportActual::Memory(tt) => {
                connect_outbound(tt, remote_peer_network_id, network_address, config, apps, handle.clone(), peers_and_metadata, peer_senders, network_context).await
            }
        }
    }
}


async fn connect_outbound<TTransport, TSocket>(
    transport: &AptosNetTransport<TTransport>,
    remote_peer_network_id: PeerNetworkId,
    addr: NetworkAddress,
    config: &NetworkConfig,
    apps: Arc<ApplicationCollector>,
    handle: Handle,
    peers_and_metadata: Arc<PeersAndMetadata>,
    peer_senders: Arc<OutboundPeerConnections>,
    network_context: NetworkContext,
) -> io::Result<()>
    where
        TSocket: crate::transport::TSocket,
        TTransport: Transport<Output = TSocket, Error = io::Error> + Send + 'static,
{
    info!("dial connect_outbound {:?}", addr);
    let peer_id = remote_peer_network_id.peer_id();
    // TODO: rebuild connection init time counter
    let outbound = match transport.dial(peer_id, addr.clone()) {
        Ok(outbound) => {
            outbound
        }
        Err(err) => {
            warn!("dial err: {:?}", err);
            // TODO: counter
            return Err(err);
        }
    };
    let mut connection = match outbound.await {
        Ok(connection) => { // Connection<TSocket>
            connection
        }
        Err(err) => {
            warn!("dial err 2: {:?}", err);
            // TODO: counter
            return Err(err);
        }
    };
    let dialed_peer_id = connection.metadata.remote_peer_id;
    if dialed_peer_id != peer_id {
        warn!("dial {:?} did not reach peer {:?} but peer {:?}", addr, peer_id, dialed_peer_id);
        _ = connection.socket.close().await; // discard secondary close error
        return Err(Error::new(ErrorKind::InvalidData, "peer_id mismatch"));
    }
    info!("dial starting peer {:?}", addr);
    peer::start_peer(
        config,
        connection.socket,
        connection.metadata,
        apps,
        handle,
        remote_peer_network_id,
        peers_and_metadata,
        peer_senders,
        network_context,
    );
    Ok(())
}
