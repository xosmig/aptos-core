// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use aptos_config::network_id::PeerNetworkId;
use aptos_network2::{
    application::interface::NetworkEvents,
    protocols::network::{Event, RpcError},
    ProtocolId,
};
use aptos_storage_service_types::{
    requests::StorageServiceRequest, responses::StorageServiceResponse, Result,
    StorageServiceMessage,
};
use bytes::Bytes;
use futures::{
    channel::oneshot,
    future,
    stream::{BoxStream, Stream, StreamExt},
};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::runtime::Handle;
use tokio_stream::wrappers::ReceiverStream;
use aptos_network2::protocols::network::network_event_prefetch;

/// A simple wrapper for each network request
pub struct NetworkRequest {
    pub peer_network_id: PeerNetworkId,
    pub protocol_id: ProtocolId,
    pub storage_service_request: StorageServiceRequest,
    pub response_sender: ResponseSender,
}

/// A stream of requests from network. Each request also comes with a callback to
/// send the response.
pub struct StorageServiceNetworkEvents {
    network_request_stream: BoxStream<'static, NetworkRequest>,
}

impl StorageServiceNetworkEvents {
    pub fn new(network_events: NetworkEvents<StorageServiceMessage>, handle: &Handle) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(10); // TODO: configurable prefetch size other than 10?
        handle.spawn(network_event_prefetch(network_events, event_tx));
        let network_events = ReceiverStream::new(event_rx);

        // Transform each event to a network request
        let network_request_stream = network_events
            .filter_map(|event| {
                future::ready(Self::event_to_request(event))
            })
            .boxed();

        Self {
            network_request_stream,
        }
    }

    /// Filters out everything except Rpc requests
    fn event_to_request(
        // network_id: NetworkId,
        event: Event<StorageServiceMessage>,
    ) -> Option<NetworkRequest> {
        match event {
            Event::RpcRequest(
                peer_network_id,
                StorageServiceMessage::Request(storage_service_request),
                protocol_id,
                response_tx,
            ) => {
                let response_sender = ResponseSender::new(response_tx);
                // let peer_network_id = PeerNetworkId::new(network_id, peer_id);
                Some(NetworkRequest {
                    peer_network_id,
                    protocol_id,
                    storage_service_request,
                    response_sender,
                })
            },
            _ => None, // We don't use direct send and don't care about connection events
        }
    }
}

impl Stream for StorageServiceNetworkEvents {
    type Item = NetworkRequest;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.network_request_stream).poll_next(cx)
    }
}

/// A channel for fulfilling a pending StorageService RPC request.
/// Provides a more strongly typed interface around the raw RPC response channel.
pub struct ResponseSender {
    response_tx: oneshot::Sender<Result<Bytes, RpcError>>,
}

impl ResponseSender {
    pub fn new(response_tx: oneshot::Sender<Result<Bytes, RpcError>>) -> Self {
        Self { response_tx }
    }

    pub fn send(self, response: Result<StorageServiceResponse>) {
        let msg = StorageServiceMessage::Response(response);
        let result = bcs::to_bytes(&msg)
            .map(Bytes::from)
            .map_err(RpcError::BcsError);
        let _ = self.response_tx.send(result);
    }
}
