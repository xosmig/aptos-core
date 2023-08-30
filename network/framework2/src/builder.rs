// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::runtime::Handle;
use aptos_config::config::{NetworkConfig, RoleType};
use aptos_config::network_id::{NetworkContext, PeerNetworkId};
use aptos_event_notifications::{DbBackedOnChainConfig,EventSubscriptionService};
use aptos_network_discovery::DiscoveryChangeListener;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use crate::application::storage::PeersAndMetadata;
use crate::protocols::wire::handshake::v1::ProtocolId;
use crate::protocols::wire::messaging::v1::NetworkMessage;

#[derive(Debug, PartialEq, PartialOrd)]
enum State {
    CREATED,
    BUILT,
    STARTED,
}

/// Build Network module with custom configuration values.
/// Methods can be chained in order to set the configuration values.
/// MempoolNetworkHandler and ConsensusNetworkHandler are constructed by calling
/// [`NetworkBuilder::build`].  New instances of `NetworkBuilder` are obtained
/// via [`NetworkBuilder::create`].
pub struct NetworkBuilder {
    state: State,
    executor: Option<Handle>,
    time_service: TimeService,
    network_context: NetworkContext,
    discovery_listeners: Option<Vec<DiscoveryChangeListener<DbBackedOnChainConfig>>>,
    // connectivity_manager_builder: Option<ConnectivityManagerBuilder>, // TODO network2: re-enable connectivity manager
    // health_checker_builder: Option<HealthCheckerBuilder>,
    // peer_manager_builder: PeerManagerBuilder,
    peers_and_metadata: Arc<PeersAndMetadata>,
}

impl NetworkBuilder {
    /// Create a new NetworkBuilder based on the provided configuration.
    pub fn create(
        chain_id: ChainId,
        role: RoleType,
        config: &NetworkConfig,
        time_service: TimeService,
        mut reconfig_subscription_service: Option<&mut EventSubscriptionService>,
        peers_and_metadata: Arc<PeersAndMetadata>,
    ) -> NetworkBuilder {
        let peer_id = config.peer_id();
        let network_context = NetworkContext::new(role, config.network_id, peer_id);
        NetworkBuilder{
            state: State::CREATED,
            executor: None,
            time_service,
            network_context,
            discovery_listeners: None,
            // connectivity_manager_builder: None,
            peers_and_metadata,
        }
    }

    pub fn build(&mut self, handle: Handle) {
        if self.state != State::CREATED {
            panic!("NetworkBuilder.build but not in state CREATED");
        }
        self.state = State::BUILT;
    }

    pub fn start(&mut self) {
        // TODO network2 start the built network
    }

    pub fn network_context(&self) -> NetworkContext {
        self.network_context.clone()
    }
}

pub struct ReceivedMessage {
    pub message: NetworkMessage,
    pub sender: PeerNetworkId,
}

impl ReceivedMessage {
    pub fn protocol_id(&self) -> Option<ProtocolId> {
        match &self.message {
            NetworkMessage::Error(e) => {
                None
            }
            NetworkMessage::RpcRequest(req) => {
                Some(req.protocol_id)
            }
            NetworkMessage::RpcResponse(response) => {
                // TODO network2: mis-design of RpcResponse lacking ProtocolId requires global rpc counter (or at least per-peer)
                None
            }
            NetworkMessage::DirectSendMsg(msg) => {
                Some(msg.protocol_id)
            }
        }
    }
}

pub struct ApplicationConnections {
    pub protocol_id: ProtocolId,
    pub sender: tokio::sync::mpsc::Sender<ReceivedMessage>,
    pub receiver: tokio::sync::mpsc::Receiver<ReceivedMessage>,
}

pub struct ApplicationCollector {
    apps: BTreeMap<ProtocolId,ApplicationConnections>,
}
