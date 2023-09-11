// Copyright © Aptos Foundation

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::runtime::Runtime;
use aptos_config::config::{NetworkConfig, NodeConfig};
use aptos_config::network_id::NetworkId;
use aptos_consensus::network_interface::ConsensusMsg;
use aptos_network2::protocols::wire::handshake::v1::ProtocolId;
use aptos_network2_builder::{ApplicationCollector,ApplicationConnections,NetworkBuilder};
// use aptos_consensus::network_interface::{DIRECT_SEND, RPC};
use aptos_logger::debug;
use aptos_network2::application::interface::{NetworkClient,NetworkMessageTrait};
use aptos_network2::protocols::network::{NetworkEvents,NetworkSender,NetworkSource,NewNetworkSender,ReceivedMessage,Message};
use aptos_network2::application::storage::PeersAndMetadata;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use aptos_event_notifications::EventSubscriptionService;
use aptos_peer_monitoring_service_types::PeerMonitoringServiceMessage;
use aptos_storage_service_types::StorageServiceMessage;
use aptos_mempool::MempoolSyncMsg;

pub trait MessageTrait : Clone + DeserializeOwned + Serialize + Send + Sync + Unpin + 'static {}
impl<T: Clone + DeserializeOwned + Serialize + Send + Sync + Unpin + 'static> MessageTrait for T {}

/// A simple struct that holds both the network client
/// and receiving interfaces for an application.
pub struct ApplicationNetworkInterfaces<T> {
    pub network_client: NetworkClient<T>,
    pub network_events: NetworkEvents<T>,
}

pub struct Protocols {
    pub direct_send_protocols_and_preferences: Vec<ProtocolId>,
    pub rpc_protocols_and_preferences: Vec<ProtocolId>,
}

pub fn consensus_protocols() -> Protocols {
    Protocols {
        direct_send_protocols_and_preferences: aptos_consensus::network_interface::DIRECT_SEND.into(),
        rpc_protocols_and_preferences: aptos_consensus::network_interface::RPC.into(),
    }
}

pub fn mempool_protocols() -> Protocols {
Protocols {
    direct_send_protocols_and_preferences: vec![ProtocolId::MempoolDirectSend],
    rpc_protocols_and_preferences: vec![],
}
}

pub fn peer_monitoring_protocols() -> Protocols {
    Protocols {
        direct_send_protocols_and_preferences: vec![],
        rpc_protocols_and_preferences: vec![ProtocolId::PeerMonitoringServiceRpc],
    }
}

pub fn storage_service_protocols() -> Protocols {
    Protocols {
        direct_send_protocols_and_preferences: vec![],
        rpc_protocols_and_preferences: vec![ProtocolId::StorageServiceRpc],
    }
}

impl<T: MessageTrait> ApplicationNetworkInterfaces<T> {
    pub fn new(
        direct_send_protocols_and_preferences: Vec<ProtocolId>,
        rpc_protocols_and_preferences: Vec<ProtocolId>,
        peers_and_metadata: Arc<PeersAndMetadata>,
        // receive: tokio::sync::mpsc::Receiver<ReceivedMessage>,
        network_source: NetworkSource,
        queue_size: usize,
    ) -> Self {
        let mut network_senders = HashMap::new();
        network_senders.insert(NetworkId::Validator, NetworkSender::new());
        network_senders.insert(NetworkId::Internal, NetworkSender::new());
        network_senders.insert(NetworkId::Vfn, NetworkSender::new());
        network_senders.insert(NetworkId::Public, NetworkSender::new());
        let network_client = NetworkClient::new(
            direct_send_protocols_and_preferences,
            rpc_protocols_and_preferences,
            network_senders,
            peers_and_metadata,
        );
        let network_events = NetworkEvents::new(network_source);
        Self {
            network_client,
            network_events,
        }
    }
}

fn has_validator_network(node_config: &NodeConfig) -> bool {
    for net_config in node_config.full_node_networks.iter() {
        if net_config.network_id.is_validator_network() {
            return true;
        }
    }
    return false;
}

fn build_network_connections<T: MessageTrait>(
    direct_send_protocols : Vec<ProtocolId>,
    rpc_protocols : Vec<ProtocolId>,
    queue_size: usize,
    counter_label: &str,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: &mut ApplicationCollector,
) -> ApplicationNetworkInterfaces<T> {
    // TODO: pack a map {ProtocolId: Receiver, ...} and allow app code to unpack that
    // let prots = BTreeMap::new();
    let mut receivers = vec![];

    for protocol_id in direct_send_protocols.iter() {
        let (app_con, receiver) = ApplicationConnections::build(*protocol_id, queue_size, counter_label);
        // prots.insert(*protocol_id, receiver);
        receivers.push(receiver);
        apps.add(app_con);
    }
    for protocol_id in rpc_protocols.iter() {
        let (app_con, receiver) = ApplicationConnections::build(*protocol_id, queue_size, counter_label);
        // prots.insert(*protocol_id, receiver);
        receivers.push(receiver);
        apps.add(app_con);
    }

    let network_source = if receivers.len() == 1 {
        NetworkSource::new_single_source(receivers.remove(0))
    } else if receivers.len() > 1 {
        NetworkSource::new_multi_source(receivers)
    } else {
        panic!("{:?} built no receivers", counter_label);
    };
    ApplicationNetworkInterfaces::new(
        direct_send_protocols,
        rpc_protocols,
        peers_and_metadata,
        network_source,
        queue_size)
}

pub fn consensus_network_connections(
    node_config: &NodeConfig,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: &mut ApplicationCollector
) -> Option<ApplicationNetworkInterfaces<ConsensusMsg>> {
    if !has_validator_network(node_config) {
        return None;
    }

    let direct_send_protocols: Vec<ProtocolId> = aptos_consensus::network_interface::DIRECT_SEND.into();
    let rpc_protocols: Vec<ProtocolId> = aptos_consensus::network_interface::RPC.into();
    let queue_size = node_config.consensus.max_network_channel_size;
    let counter_label = "consensus";

    Some(build_network_connections(direct_send_protocols, rpc_protocols, queue_size, counter_label, peers_and_metadata, apps))
}

pub fn peer_monitoring_network_connections(
    node_config: &NodeConfig,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: &mut ApplicationCollector
) -> ApplicationNetworkInterfaces<PeerMonitoringServiceMessage> {
    let direct_send_protocols = Vec::<ProtocolId>::new();
    let rpc_protocols = vec![ProtocolId::PeerMonitoringServiceRpc];
    let queue_size = node_config.peer_monitoring_service.max_network_channel_size as usize;
    let counter_label = "peer_monitoring";

    build_network_connections(direct_send_protocols, rpc_protocols, queue_size, counter_label, peers_and_metadata, apps)
}

pub fn storage_service_network_connections(
    node_config: &NodeConfig,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: &mut ApplicationCollector
) -> ApplicationNetworkInterfaces<StorageServiceMessage> {
    let direct_send_protocols = Vec::<ProtocolId>::new();
    let rpc_protocols = vec![ProtocolId::StorageServiceRpc];
    let queue_size = node_config.state_sync.storage_service.max_network_channel_size as usize;
    let counter_label = "storage_service";

    build_network_connections(direct_send_protocols, rpc_protocols, queue_size, counter_label, peers_and_metadata, apps)
}

pub fn mempool_network_connections(
    node_config: &NodeConfig,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: &mut ApplicationCollector
) -> ApplicationNetworkInterfaces<MempoolSyncMsg> {
    let direct_send_protocols = vec![ProtocolId::MempoolDirectSend];
    let rpc_protocols = vec![];
    let queue_size = node_config.mempool.max_network_channel_size;
    let counter_label = "mempool";

    build_network_connections(direct_send_protocols, rpc_protocols, queue_size, counter_label, peers_and_metadata, apps)
}

/// Creates a network runtime for the given network config
pub fn create_network_runtime(network_config: &NetworkConfig) -> Runtime {
    let network_id = network_config.network_id;
    debug!("Creating runtime for network ID: {}", network_id);

    // Create the runtime
    let thread_name = format!(
        "network-{}",
        network_id.as_str().chars().take(3).collect::<String>()
    );
    aptos_runtimes::spawn_named_runtime(thread_name, network_config.runtime_threads)
}

/// Extracts all network configs from the given node config
fn extract_network_configs(node_config: &NodeConfig) -> Vec<NetworkConfig> {
    let mut network_configs: Vec<NetworkConfig> = node_config.full_node_networks.to_vec();
    if let Some(network_config) = node_config.validator_network.as_ref() {
        // Ensure that mutual authentication is enabled by default!
        if !network_config.mutual_authentication {
            panic!("Validator networks must always have mutual_authentication enabled!");
        }
        network_configs.push(network_config.clone());
    }
    network_configs
}

/// Extracts all network ids from the given node config
fn extract_network_ids(node_config: &NodeConfig) -> Vec<NetworkId> {
    extract_network_configs(node_config)
        .into_iter()
        .map(|network_config| network_config.network_id)
        .collect()
}

/// Creates the global peers and metadata struct
pub fn create_peers_and_metadata(node_config: &NodeConfig) -> Arc<PeersAndMetadata> {
    let network_ids = extract_network_ids(node_config);
    PeersAndMetadata::new(&network_ids)
}

pub fn setup_networks(
    node_config: &NodeConfig,
    chain_id: ChainId,
    peers_and_metadata: Arc<PeersAndMetadata>,
    event_subscription_service: &mut EventSubscriptionService,
) -> (Vec<Runtime>, Vec<NetworkBuilder>) {
    let network_configs = extract_network_configs(node_config);

    let mut network_runtimes = vec![];
    let mut networks = vec![];

    for network_config in network_configs.into_iter() {
        // Create a network runtime for the config
        let runtime = create_network_runtime(&network_config);

        // Entering gives us a runtime to instantiate all the pieces of the builder
        let _enter = runtime.enter();

        // Create a new network builder
        let mut network_builder = NetworkBuilder::create(
            chain_id,
            node_config.base.role,
            &network_config,
            TimeService::real(),
            Some(event_subscription_service),
            peers_and_metadata.clone(),
        );

        // Register consensus (both client and server) with the network
        // let network_id = network_config.network_id;
        // if network_id.is_validator_network() {}
        // Build and start the network on the runtime
        network_builder.build(runtime.handle().clone());
        debug!(
            "Network built for the network context: {}",
            network_builder.network_context()
        );
        network_runtimes.push(runtime);
        networks.push(network_builder);
    }

    (network_runtimes, networks)
}
