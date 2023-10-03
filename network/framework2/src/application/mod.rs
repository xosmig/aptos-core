// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use crate::ProtocolId;
use crate::protocols::network::ReceivedMessage;

pub mod error;
pub mod interface;
pub mod metadata;
pub mod storage;

/// Container for connection to application code listening on a ProtocolId
pub struct ApplicationConnections {
    pub protocol_id: ProtocolId,

    /// sender receives messages from network, towards application code
    pub sender: tokio::sync::mpsc::Sender<ReceivedMessage>,

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

/// Routing by ProtocolId for all application code built into a node.
/// Typically built early in startup code and then read-only.
pub struct ApplicationCollector {
    pub apps: BTreeMap<ProtocolId,ApplicationConnections>,
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

#[cfg(test)]
mod tests;
