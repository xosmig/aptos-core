// Copyright © Aptos Foundation

use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;
use crate::ApplicationConnections;

/// Peer holds what is needed by the write thread
pub struct Peer {
    receiver: Receiver<NetworkMessage>,
    apps: Arc<ApplicationConnections>,
}
