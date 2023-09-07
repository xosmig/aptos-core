// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Convenience Network API for Aptos

pub use crate::protocols::rpc::error::RpcError;
use crate::{
    error::NetworkError,
    // peer_manager::{
    //     ConnectionNotification, ConnectionRequestSender, PeerManagerNotification,
    //     PeerManagerRequestSender,
    // },
    transport::ConnectionMetadata,
    ProtocolId,
};
// use aptos_channels::aptos_channel;
use aptos_logger::prelude::*;
use aptos_short_hex_str::AsShortHexStr;
use aptos_types::{network_address::NetworkAddress, PeerId};
use bytes::Bytes;
use futures::{
    channel::oneshot,
    future,
    stream::{FilterMap, FusedStream, Map, Select, Stream, StreamExt},
    task::{Context, Poll},
};
use pin_project::pin_project;
use serde::{de::DeserializeOwned, Serialize};
use std::{cmp::min, fmt::Debug, marker::PhantomData, pin::Pin, time::Duration};
use std::collections::BTreeMap;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_stream::wrappers::ReceiverStream;
use aptos_config::network_id::{NetworkId, PeerNetworkId};
use crate::protocols::wire::messaging::v1::{NetworkMessage, RequestId};

pub trait Message: DeserializeOwned + Serialize {}
impl<T: DeserializeOwned + Serialize> Message for T {}

/// Events received by network clients in a validator
///
/// An enumeration of the various types of messages that the network will be sending
/// to its clients. This differs from [`PeerNotification`] since the contents are deserialized
/// into the type `TMessage` over which `Event` is generic. Note that we assume here that for every
/// consumer of this API there's a singleton message type, `TMessage`,  which encapsulates all the
/// messages and RPCs that are received by that consumer.
///
/// [`PeerNotification`]: crate::peer::PeerNotification
#[derive(Debug)]
pub enum Event<TMessage> {
    /// New inbound direct-send message from peer.
    Message(PeerNetworkId, TMessage),
    /// New inbound rpc request. The request is fulfilled by sending the
    /// serialized response `Bytes` over the `oneshot::Sender`, where the network
    /// layer will handle sending the response over-the-wire.
    RpcRequest(
        PeerNetworkId,
        TMessage,
        ProtocolId,
        oneshot::Sender<Result<Bytes, RpcError>>,
    ),
    /// Peer which we have a newly established connection with. TODO network2: remove this from Event?
    NewPeer(ConnectionMetadata),
    /// Peer with which we've lost our connection. TODO network2: remove this from Event?
    LostPeer(ConnectionMetadata),
}

/// impl PartialEq for simpler testing
impl<TMessage: PartialEq> PartialEq for Event<TMessage> {
    fn eq(&self, other: &Event<TMessage>) -> bool {
        use Event::*;
        match (self, other) {
            (Message(pid1, msg1), Message(pid2, msg2)) => pid1 == pid2 && msg1 == msg2,
            // ignore oneshot::Sender in comparison
            (RpcRequest(pid1, msg1, proto1, _), RpcRequest(pid2, msg2, proto2, _)) => {
                pid1 == pid2 && msg1 == msg2 && proto1 == proto2
            },
            (NewPeer(metadata1), NewPeer(metadata2)) => metadata1 == metadata2,
            (LostPeer(metadata1), LostPeer(metadata2)) => metadata1 == metadata2,
            _ => false,
        }
    }
}


// TODO: delete NetworkClientConfig as redundant, same as 'service' config
// /// Configuration needed for the client side of AptosNet applications
// #[derive(Clone)]
// pub struct NetworkClientConfig {
//     /// Direct send protocols for the application (sorted by preference, highest to lowest)
//     pub direct_send_protocols_and_preferences: Vec<ProtocolId>,
//     /// RPC protocols for the application (sorted by preference, highest to lowest)
//     pub rpc_protocols_and_preferences: Vec<ProtocolId>,
// }
//
// impl NetworkClientConfig {
//     pub fn new(
//         direct_send_protocols_and_preferences: Vec<ProtocolId>,
//         rpc_protocols_and_preferences: Vec<ProtocolId>,
//     ) -> Self {
//         Self {
//             direct_send_protocols_and_preferences,
//             rpc_protocols_and_preferences,
//         }
//     }
// }

const DEFAULT_QUEUE_SIZE : usize = 1000;

#[derive(Clone)]
pub struct ApplicationProtocolConfig{
    pub protocol_id : ProtocolId,
    pub queue_size : usize,
}

/// Configuration needed for the service side of AptosNet applications
#[derive(Clone)]
pub struct NetworkApplicationConfig {
    /// Direct send protocols for the application (sorted by preference, highest to lowest)
    pub direct_send_protocols_and_preferences: Vec<ApplicationProtocolConfig>,
    /// RPC protocols for the application (sorted by preference, highest to lowest)
    pub rpc_protocols_and_preferences: Vec<ApplicationProtocolConfig>,
    /// Which networks do we want traffic from? [] for all.
    pub networks: Vec<NetworkId>,
}

fn default_protocol_configs(protocol_ids: Vec<ProtocolId>) -> Vec<ApplicationProtocolConfig> {
    protocol_ids.iter().map(|protocol_id| ApplicationProtocolConfig{protocol_id: *protocol_id, queue_size: DEFAULT_QUEUE_SIZE}).collect()
}

fn protocol_configs_for_queue_size(protocol_ids: Vec<ProtocolId>, queue_size: usize) -> Vec<ApplicationProtocolConfig> {
    protocol_ids.iter().map(|protocol_id| ApplicationProtocolConfig{protocol_id: *protocol_id, queue_size}).collect()
}

impl NetworkApplicationConfig {
    /// New NetworkApplicationConfig which talks to all peers on all networks
    pub fn new(
        direct_send_protocols_and_preferences: Vec<ProtocolId>,
        rpc_protocols_and_preferences: Vec<ProtocolId>,
    ) -> Self {
        Self {
            direct_send_protocols_and_preferences: default_protocol_configs(direct_send_protocols_and_preferences),
            rpc_protocols_and_preferences: default_protocol_configs(rpc_protocols_and_preferences),
            networks: vec![],
        }
    }
    /// New NetworkApplicationConfig which talks to peers only on selected networks
    pub fn new_for_networks(
        direct_send_protocols_and_preferences: Vec<ProtocolId>,
        rpc_protocols_and_preferences: Vec<ProtocolId>,
        networks: Vec<NetworkId>,
        queue_size: usize,
    ) -> Self {
        Self {
            direct_send_protocols_and_preferences: protocol_configs_for_queue_size(direct_send_protocols_and_preferences, queue_size),
            rpc_protocols_and_preferences: protocol_configs_for_queue_size(rpc_protocols_and_preferences, queue_size),
            networks,
        }
    }

    pub fn wants_network(&self, network: NetworkId) -> bool {
        self.networks.is_empty() || self.networks.contains(&network)
    }
}

// TODO network2: is this the final home of ReceivedMessage? better place to put it?
pub struct ReceivedMessage {
    pub message: NetworkMessage,
    pub sender: PeerNetworkId,
}

impl ReceivedMessage {
    pub fn protocol_id(&self) -> Option<ProtocolId> {
        match &self.message {
            NetworkMessage::Error(_e) => {
                None
            }
            NetworkMessage::RpcRequest(req) => {
                Some(req.protocol_id)
            }
            NetworkMessage::RpcResponse(_response) => {
                // TODO network2: legacy design of RpcResponse lacking ProtocolId requires global rpc counter (or at least per-peer) and requires reply matching globally or per-peer
                None
            }
            NetworkMessage::DirectSendMsg(msg) => {
                Some(msg.protocol_id)
            }
        }
    }
}

// /// Configuration needed for AptosNet applications to register with the network
// /// builder. Supports client and service side.
// #[derive(Clone)]
// pub struct NetworkApplicationConfig { // TODO: delete? obsolete wrapper since client config is the same as service?
//     // pub network_client_config: NetworkClientConfig,
//     pub network_service_config: NetworkServiceConfig,
// }
//
// impl NetworkApplicationConfig {
//     pub fn new(
//         // network_client_config: NetworkClientConfig,
//         network_service_config: NetworkServiceConfig,
//     ) -> Self {
//         Self {
//             // network_client_config,
//             network_service_config,
//         }
//     }
// }

struct OpenRpcRequestState {
    id: RequestId,
    sender: oneshot::Sender<Result<Bytes, RpcError>>,
    protocol_id: ProtocolId,
}

pub struct NetworkEvents<TMessage> {
    network_source: NetworkSource, //::sync::mpsc::Receiver<ReceivedMessage>,
    done: bool,
    // TODO network2: add rpc-response matching at the application level here
    open_outbound_rpc: BTreeMap<RequestId, OpenRpcRequestState>,

    // TMessage is the type we will deserialize to
    phantom: PhantomData<TMessage>,
}

impl<TMessage: Message + Unpin> NetworkEvents<TMessage> {
    pub fn new(network_source: NetworkSource) -> Self {
        Self {
            network_source,
            done: false,
            open_outbound_rpc: BTreeMap::new(),
            phantom: Default::default(),
        }
    }
}

impl<TMessage: Message + Unpin> Stream for NetworkEvents<TMessage> {
    type Item = Event<TMessage>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        // throw away up to 10 messages while looking for one to return
        let mself = self.get_mut();
        for _ in 1..10 {
            let msg = match Pin::new(&mut mself.network_source).poll_next(cx) {
                Poll::Ready(x) => match x {
                    Some(msg) => {
                        msg
                    }
                    None => {
                        mself.done = true;
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => {
                    return Poll::Pending
                }
            };
            match msg.message {
                    NetworkMessage::Error(err) => {
                        // We just drop error responses! TODO: never send them
                        // fall through, maybe get another
                    }
                    NetworkMessage::RpcRequest(request) => {
                        let app_msg = match request.protocol_id.from_bytes(request.raw_request.as_slice()) {
                            Ok(x) => { x },
                            Err(_) => {
                                mself.done = true;
                                // TODO network2: log error, count error, close connection
                                return Poll::Ready(None);
                            }
                        };
                        let (responder, _response_reader) = oneshot::channel();//oneshot::Sender<Result<Bytes, RpcError>>
                        // TODO network2: add rpc-response matching at the application level here
                        return Poll::Ready(Some(Event::RpcRequest(
                            msg.sender, app_msg, request.protocol_id, responder)))
                    }
                    NetworkMessage::RpcResponse(response) => {
                        // TODO network2: add rpc-response matching at the application level here
                        let request_state = mself.open_outbound_rpc.remove(&response.request_id);
                        let request_state = match request_state {
                            None => {
                                // timeout or garbage collection or something. drop response.
                                // TODO network2 log/count dropped response
                                // TODO: drop this one message, but go back to local loop
                                return Poll::Pending;
                            }
                            Some(x) => { x }
                        };
                        let app_msg = match request_state.protocol_id.from_bytes(response.raw_response.as_slice()) {
                            Ok(x) => { x },
                            Err(_) => {
                                mself.done = true;
                                // TODO network2: log error, count error, close connection
                                return Poll::Ready(None);
                            }
                        };
                        request_state.sender.send(Ok(response.raw_response.into()));
                        // we processed a legit message, even if it didn't come back through the expected channel, yield
                        return Poll::Pending
                    }
                    NetworkMessage::DirectSendMsg(message) => {
                        let app_msg = match message.protocol_id.from_bytes(message.raw_msg.as_slice()) {
                            Ok(x) => { x },
                            Err(_) => {
                                mself.done = true;
                                // TODO network2: log error, count error, close connection
                                return Poll::Ready(None);
                            }
                        };
                        return Poll::Ready(Some(Event::Message(msg.sender, app_msg)))
                    }
                }
        }
        Poll::Pending
    }
}

impl<TMessage: Message + Unpin> FusedStream for NetworkEvents<TMessage> {
    fn is_terminated(&self) -> bool {
        self.done
    }
}

// /// A `Stream` of `Event<TMessage>` from the lower network layer to an upper
// /// network application that deserializes inbound network direct-send and rpc
// /// messages into `TMessage`. Inbound messages that fail to deserialize are logged
// /// and dropped.
// ///
// /// `NetworkEvents` is really just a thin wrapper around a
// /// `channel::Receiver<PeerNotification>` that deserializes inbound messages.
// #[pin_project]
// pub struct NetworkEvents<TMessage> {
//     #[pin]
//     event_stream: Select<
//         FilterMap<
//             aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerNotification>,
//             future::Ready<Option<Event<TMessage>>>,
//             fn(PeerManagerNotification) -> future::Ready<Option<Event<TMessage>>>,
//         >,
//         Map<
//             aptos_channel::Receiver<PeerId, ConnectionNotification>,
//             fn(ConnectionNotification) -> Event<TMessage>,
//         >,
//     >,
//     _marker: PhantomData<TMessage>,
// }
//
// /// Trait specifying the signature for `new()` `NetworkEvents`
// pub trait NewNetworkEvents {
//     fn new(
//         peer_mgr_notifs_rx: aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerNotification>,
//         connection_notifs_rx: aptos_channel::Receiver<PeerId, ConnectionNotification>,
//     ) -> Self;
// }
//
// impl<TMessage: Message> NewNetworkEvents for NetworkEvents<TMessage> {
//     fn new(
//         peer_mgr_notifs_rx: aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerNotification>,
//         connection_notifs_rx: aptos_channel::Receiver<PeerId, ConnectionNotification>,
//     ) -> Self {
//         let data_event_stream = peer_mgr_notifs_rx.filter_map(
//             peer_mgr_notif_to_event
//                 as fn(PeerManagerNotification) -> future::Ready<Option<Event<TMessage>>>,
//         );
//         let control_event_stream = connection_notifs_rx
//             .map(control_msg_to_event as fn(ConnectionNotification) -> Event<TMessage>);
//         Self {
//             event_stream: ::futures::stream::select(data_event_stream, control_event_stream),
//             _marker: PhantomData,
//         }
//     }
// }
//
// impl<TMessage> Stream for NetworkEvents<TMessage> {
//     type Item = Event<TMessage>;
//
//     fn poll_next(self: Pin<&mut Self>, context: &mut Context) -> Poll<Option<Self::Item>> {
//         self.project().event_stream.poll_next(context)
//     }
//
//     fn size_hint(&self) -> (usize, Option<usize>) {
//         self.event_stream.size_hint()
//     }
// }
//
// /// Deserialize inbound direct send and rpc messages into the application `TMessage`
// /// type, logging and dropping messages that fail to deserialize.
// fn peer_mgr_notif_to_event<TMessage: Message>(
//     notif: PeerManagerNotification,
// ) -> future::Ready<Option<Event<TMessage>>> {
//     let maybe_event = match notif {
//         PeerManagerNotification::RecvRpc(peer_id, rpc_req) => {
//             request_to_network_event(peer_id, &rpc_req)
//                 .map(|msg| Event::RpcRequest(peer_id, msg, rpc_req.protocol_id, rpc_req.res_tx))
//         },
//         PeerManagerNotification::RecvMessage(peer_id, request) => {
//             request_to_network_event(peer_id, &request).map(|msg| Event::Message(peer_id, msg))
//         },
//     };
//     future::ready(maybe_event)
// }
//
// /// Converts a `SerializedRequest` into a network `Event` for sending to other nodes
// fn request_to_network_event<TMessage: Message, Request: SerializedRequest>(
//     peer_id: PeerId,
//     request: &Request,
// ) -> Option<TMessage> {
//     match request.to_message() {
//         Ok(msg) => Some(msg),
//         Err(err) => {
//             let data = &request.data();
//             warn!(
//                 SecurityEvent::InvalidNetworkEvent,
//                 error = ?err,
//                 remote_peer_id = peer_id.short_str(),
//                 protocol_id = request.protocol_id(),
//                 data_prefix = hex::encode(&data[..min(16, data.len())]),
//             );
//             None
//         },
//     }
// }
//
// fn control_msg_to_event<TMessage>(notif: ConnectionNotification) -> Event<TMessage> {
//     match notif {
//         ConnectionNotification::NewPeer(metadata, _context) => Event::NewPeer(metadata),
//         ConnectionNotification::LostPeer(metadata, _context, _reason) => Event::LostPeer(metadata),
//     }
// }
//
// impl<TMessage> FusedStream for NetworkEvents<TMessage> {
//     fn is_terminated(&self) -> bool {
//         self.event_stream.is_terminated()
//     }
// }
//
/// `NetworkSender` is the generic interface from upper network applications to
/// the lower network layer. It provides the full API for network applications,
/// including sending direct-send messages, sending rpc requests, as well as
/// dialing or disconnecting from peers and updating the list of accepted public
/// keys.
///
/// `NetworkSender` is in fact a thin wrapper around a `PeerManagerRequestSender`, which in turn is
/// a thin wrapper on `aptos_channel::Sender<(PeerId, ProtocolId), PeerManagerRequest>`,
/// mostly focused on providing a more ergonomic API. However, network applications will usually
/// provide their own thin wrapper around `NetworkSender` that narrows the API to the specific
/// interface they need.
///
/// Provide Protobuf wrapper over `[peer_manager::PeerManagerRequestSender]`
#[derive(Clone, Debug)]
pub struct NetworkSender<TMessage> {
    // TODO: rebuild NetworkSender around single-level network::framework2
    // peer_mgr_reqs_tx: PeerManagerRequestSender,
    // connection_reqs_tx: ConnectionRequestSender,
    _marker: PhantomData<TMessage>,
}

/// Trait specifying the signature for `new()` `NetworkSender`s
pub trait NewNetworkSender {
    fn new(
        // peer_mgr_reqs_tx: PeerManagerRequestSender,
        // connection_reqs_tx: ConnectionRequestSender,
    ) -> Self;
}

impl<TMessage> NewNetworkSender for NetworkSender<TMessage> {
    fn new(
        // peer_mgr_reqs_tx: PeerManagerRequestSender,
        // connection_reqs_tx: ConnectionRequestSender,
    ) -> Self {
        Self {
            // peer_mgr_reqs_tx,
            // connection_reqs_tx,
            _marker: PhantomData,
        }
    }
}

impl<TMessage> NetworkSender<TMessage> {
    /// Request that a given Peer be dialed at the provided `NetworkAddress` and
    /// synchronously wait for the request to be performed.
    pub async fn dial_peer(&self, peer: PeerId, addr: NetworkAddress) -> Result<(), NetworkError> {
        // self.connection_reqs_tx.dial_peer(peer, addr).await?;
        Ok(())
    }

    /// Request that a given Peer be disconnected and synchronously wait for the request to be
    /// performed.
    pub async fn disconnect_peer(&self, peer: PeerId) -> Result<(), NetworkError> {
        // self.connection_reqs_tx.disconnect_peer(peer).await?;
        Ok(())
    }
}

impl<TMessage: Message> NetworkSender<TMessage> {
    /// Send a protobuf message to a single recipient. Provides a wrapper over
    /// `[peer_manager::PeerManagerRequestSender::send_to]`.
    pub fn send_to(
        &self,
        recipient: PeerId,
        protocol: ProtocolId,
        message: TMessage,
    ) -> Result<(), NetworkError> {
        // let mdata = protocol.to_bytes(&message)?.into();
        // self.peer_mgr_reqs_tx.send_to(recipient, protocol, mdata)?;
        Ok(())
    }

    /// Send a protobuf message to a many recipients. Provides a wrapper over
    /// `[peer_manager::PeerManagerRequestSender::send_to_many]`.
    pub fn send_to_many(
        &self,
        recipients: impl Iterator<Item = PeerId>,
        protocol: ProtocolId,
        message: TMessage,
    ) -> Result<(), NetworkError> {
        // Serialize message.
        // let mdata = protocol.to_bytes(&message)?.into();
        // self.peer_mgr_reqs_tx
        //     .send_to_many(recipients, protocol, mdata)?;
        Ok(())
    }

    /// Send a protobuf rpc request to a single recipient while handling
    /// serialization and deserialization of the request and response respectively.
    /// Assumes that the request and response both have the same message type.
    pub async fn send_rpc(
        &self,
        recipient: PeerId,
        protocol: ProtocolId,
        req_msg: TMessage,
        timeout: Duration,
    ) -> Result<TMessage, RpcError> {
        // serialize request
        // let req_data = protocol.to_bytes(&req_msg)?.into();
        // let res_data = self
        //     .peer_mgr_reqs_tx
        //     .send_rpc(recipient, protocol, req_data, timeout)
        //     .await?;
        // let res_msg: TMessage = protocol.from_bytes(&res_data)?;
        //Ok(res_msg)
        Err(RpcError::TimedOut)// TODO: implement send_rpc()
    }
}

/// Generalized functionality for any request across `DirectSend` and `Rpc`.
pub trait SerializedRequest {
    fn protocol_id(&self) -> ProtocolId;
    fn data(&self) -> &Bytes;

    /// Converts the `SerializedMessage` into its deserialized version of `TMessage` based on the
    /// `ProtocolId`.  See: [`ProtocolId::from_bytes`]
    fn to_message<TMessage: DeserializeOwned>(&self) -> anyhow::Result<TMessage> {
        self.protocol_id().from_bytes(self.data())
    }
}

// tokio::sync::mpsc::Receiver<ReceivedMessage>,

enum NetworkSourceUnion {
    SingleSource(tokio_stream::wrappers::ReceiverStream<ReceivedMessage>),
    ManySource(futures::stream::SelectAll<ReceiverStream<ReceivedMessage>>),
}

pub struct NetworkSource {
    source: NetworkSourceUnion,
}

impl NetworkSource {
    pub fn new_single_source(network_source: tokio::sync::mpsc::Receiver<ReceivedMessage>) -> Self {
        let network_source = ReceiverStream::new(network_source);
        Self {
            source: NetworkSourceUnion::SingleSource(network_source),
        }
    }

    pub fn new_multi_source(receivers: Vec<tokio::sync::mpsc::Receiver<ReceivedMessage>>) -> Self {
        Self {
            source: NetworkSourceUnion::ManySource(merge_receivers(receivers))
        }
    }
}


// tokio::sync::mpsc::Receiver<ReceivedMessage>
// tokio_stream::wrappers::ReceiverStream
//futures::stream::SelectAll
fn merge_receivers(receivers: Vec<tokio::sync::mpsc::Receiver<ReceivedMessage>>) -> futures::stream::SelectAll<ReceiverStream<ReceivedMessage>> {
    futures::stream::select_all(
        receivers.into_iter().map(|x| tokio_stream::wrappers::ReceiverStream::new(x))
    )
}

impl Stream for NetworkSource {
    type Item = ReceivedMessage;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().source {
            NetworkSourceUnion::SingleSource(rs) => { Pin::new(rs).poll_next(cx) }
            NetworkSourceUnion::ManySource(sa) => { Pin::new(sa).poll_next(cx) }
        }
    }

    // pub fn try_recv(&mut self) -> Result<ReceivedMessage, TryRecvError> {
    //     match self {
    //         NetworkSource::SingleSource(source) => {
    //             source.try_recv()
    //         }
    //         NetworkSource::ManySource(sources) => {
    //
    //         }
    //     }
    // }
}
