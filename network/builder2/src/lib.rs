// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::runtime::Handle;
use aptos_config::config::{NetworkConfig, RoleType};
use aptos_config::network_id::{NetworkContext, PeerNetworkId};
use aptos_event_notifications::{DbBackedOnChainConfig,EventSubscriptionService};
// use aptos_network_discovery::DiscoveryChangeListener;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use aptos_network2::application::storage::PeersAndMetadata;
use aptos_network2::protocols::wire::handshake::v1::ProtocolId;
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;
use aptos_network2::protocols::network::ReceivedMessage;

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
    // discovery_listeners: Option<Vec<DiscoveryChangeListener<DbBackedOnChainConfig>>>, // TODO network2: re-build known-peer-directory
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
            // discovery_listeners: None,
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

pub struct ApplicationConnections {
    pub protocol_id: ProtocolId,

    /// sender receives messages from network, towards application code
    pub sender: tokio::sync::mpsc::Sender<ReceivedMessage>,

    /// receiver is where application code takes messages from network peers
    //pub receiver: tokio::sync::mpsc::Receiver<ReceivedMessage>,

    /// label used in metrics counters
    pub label: String,
}

impl ApplicationConnections {
    pub fn build(protocol_id: ProtocolId, queue_size: usize, label: &str) -> (ApplicationConnections, tokio::sync::mpsc::Receiver<ReceivedMessage>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(queue_size);
        (ApplicationConnections {
            protocol_id,
            sender,
            label: label.to_string(),
        }, receiver)
    }
}

pub struct ApplicationCollector {
    apps: BTreeMap<ProtocolId,ApplicationConnections>,
}

impl ApplicationCollector {
    pub fn new() -> Self {
        Self {
            apps: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, connections: ApplicationConnections) {
        self.apps.insert(connections.protocol_id, connections);
    }
}
