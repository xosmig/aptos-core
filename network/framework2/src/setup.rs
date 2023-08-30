// Copyright © Aptos Foundation

use std::sync::Arc;
use tokio::runtime::Runtime;
use aptos_config::config::{NetworkConfig, NodeConfig};
use aptos_event_notifications::EventSubscriptionService;
use aptos_logger::debug;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use crate::application::storage::PeersAndMetadata;
use crate::builder::NetworkBuilder;

pub fn setup_applications() {
    // let mut wat : NetworkApplicationConfig;
    ;
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

/// Creates a network runtime for the given network config
fn create_network_runtime(network_config: &NetworkConfig) -> Runtime {
    let network_id = network_config.network_id;
    debug!("Creating runtime for network ID: {}", network_id);

    // Create the runtime
    let thread_name = format!(
        "network-{}",
        network_id.as_str().chars().take(3).collect::<String>()
    );
    aptos_runtimes::spawn_named_runtime(thread_name, network_config.runtime_threads)
}

pub fn setup_networks(
    node_config: &NodeConfig,
    chain_id: ChainId,
    peers_and_metadata: Arc<PeersAndMetadata>,
    event_subscription_service: &mut EventSubscriptionService,
) -> Vec<Runtime> { // TODO network2 return (runtimes, networks) ?
    let network_configs = extract_network_configs(node_config);

    let mut network_runtimes = vec![];

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
        let network_id = network_config.network_id;
        // if network_id.is_validator_network() {}
        // Build and start the network on the runtime
        network_builder.build(runtime.handle().clone());
        network_builder.start();
        network_runtimes.push(runtime);
        debug!(
            "Network built for the network context: {}",
            network_builder.network_context()
        );
    }

    network_runtimes
}
