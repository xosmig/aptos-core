// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//!
//! This module is to contain all networking logging information.
//!
//! ```
//! use aptos_config::network_id::NetworkContext;
//! use aptos_logger::info;
//! use aptos_types::{PeerId, network_address::NetworkAddress};
//! use aptos_network::logging::NetworkSchema;
//!
//! info!(
//!   NetworkSchema::new(&NetworkContext::mock())
//!     .remote_peer(&PeerId::random())
//!     .network_address(&NetworkAddress::mock()),
//!   field_name = "field",
//!   "Value is {} message",
//!   5
//! );
//! ```

use crate::{
    // connectivity_manager::DiscoverySource,
    transport::{ConnectionId, ConnectionMetadata},
};
use aptos_config::network_id::NetworkContext;
use aptos_logger::Schema;
use aptos_netcore::transport::ConnectionOrigin;
use aptos_num_variants::NumVariants;
use aptos_types::{network_address::NetworkAddress, PeerId};
use serde::Serialize;

// TODO: DiscoverySource moved from connectivity_manager, but might need to move back (without a bunch of stuff)

/// Different sources for peer addresses, ordered by priority (Onchain=highest,
/// Config=lowest).
#[repr(u8)]
#[derive(Copy, Clone, Eq, Hash, PartialEq, Ord, PartialOrd, NumVariants, Serialize)]
pub enum DiscoverySource {
    OnChainValidatorSet,
    File,
    Rest,
    Config,
}

impl std::fmt::Debug for DiscoverySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl std::fmt::Display for DiscoverySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            DiscoverySource::OnChainValidatorSet => "OnChainValidatorSet",
            DiscoverySource::File => "File",
            DiscoverySource::Config => "Config",
            DiscoverySource::Rest => "Rest",
        })
    }
}

#[derive(Schema)]
pub struct NetworkSchema<'a> {
    connection_id: Option<&'a ConnectionId>,
    #[schema(display)]
    connection_origin: Option<&'a ConnectionOrigin>,
    #[schema(display)]
    discovery_source: Option<&'a DiscoverySource>,
    message: Option<String>,
    #[schema(display)]
    network_address: Option<&'a NetworkAddress>,
    network_context: &'a NetworkContext,
    #[schema(display)]
    remote_peer: Option<&'a PeerId>,
}

impl<'a> NetworkSchema<'a> {
    pub fn new(network_context: &'a NetworkContext) -> Self {
        Self {
            connection_id: None,
            connection_origin: None,
            discovery_source: None,
            message: None,
            network_address: None,
            network_context,
            remote_peer: None,
        }
    }

    pub fn connection_metadata(self, metadata: &'a ConnectionMetadata) -> Self {
        self.connection_id(&metadata.connection_id)
            .connection_origin(&metadata.origin)
            .remote_peer(&metadata.remote_peer_id)
    }

    pub fn connection_metadata_with_address(self, metadata: &'a ConnectionMetadata) -> Self {
        self.connection_id(&metadata.connection_id)
            .connection_origin(&metadata.origin)
            .remote_peer(&metadata.remote_peer_id)
            .network_address(&metadata.addr)
    }
}
