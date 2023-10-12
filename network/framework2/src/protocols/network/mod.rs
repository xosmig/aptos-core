// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Convenience Network API for Aptos

pub use crate::protocols::rpc::error::RpcError;
use crate::{
    application::interface::{OutboundRpcMatcher,OpenRpcRequestState},
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
use std::any::TypeId;
use std::{cmp::min, fmt::Debug, marker::PhantomData, pin::Pin, time::Duration};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::ops::{Add, DerefMut, Sub};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LockResult, RwLock};
use anyhow::Error;
use futures::channel::oneshot::Canceled;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::{SendError, TryRecvError, TrySendError};
use tokio::sync::mpsc::Receiver;
use tokio::time::error::Elapsed;
use tokio_stream::wrappers::ReceiverStream;
use aptos_config::network_id::{NetworkId, PeerNetworkId};
// use crate::application::error::Error::RpcError;
// use aptos_infallible::RwLock;
use crate::error::NetworkErrorKind;
use crate::protocols::wire::messaging::v1::{DirectSendMsg, NetworkMessage, RequestId, RpcRequest, RpcResponse};
use hex::ToHex;

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
    // Peer connect/disconnect events are now served out of a subscription on PeersAndMetadata object
    // Peer which we have a newly established connection with. TODO network2: remove this from Event?
    // NewPeer(ConnectionMetadata),
    // Peer with which we've lost our connection. TODO network2: remove this from Event?
    // LostPeer(ConnectionMetadata),
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
            // (NewPeer(metadata1), NewPeer(metadata2)) => metadata1 == metadata2,
            // (LostPeer(metadata1), LostPeer(metadata2)) => metadata1 == metadata2,
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
#[derive(Debug)]
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
    pub fn protocol_id_as_str(&self) -> &'static str {
        match &self.message {
            NetworkMessage::Error(_) => {"error"}
            NetworkMessage::RpcRequest(rr) => {rr.protocol_id.as_str()}
            NetworkMessage::RpcResponse(_) => {"rpc response"}
            NetworkMessage::DirectSendMsg(dm) => {dm.protocol_id.as_str()}
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

// struct OpenRpcRequestState {
//     id: RequestId,
//     // send on this to deliver a reply back to an open NetworkSender.send_rpc()
//     sender: oneshot::Sender<Result<Bytes, RpcError>>,
//     protocol_id: ProtocolId,
//     // TODO? add a deadline timeout field?
// }
//
// /// OutboundRpcMatcher contains an Arc-RwLock of oneshot reply channels
// #[derive(Clone)]
// struct OutboundRpcMatcher {
//     open_outbound_rpc: Arc<RwLock<BTreeMap<RequestId, OpenRpcRequestState>>>,
// }
//
// impl OutboundRpcMatcher {
//     fn new() -> Self {
//         Self {
//             open_outbound_rpc: Arc::new(RwLock::new(BTreeMap::new()))
//         }
//     }
//
//     fn remove(&self, request_id: &RequestId) -> Option<OpenRpcRequestState> {
//         self.open_outbound_rpc.write().unwrap().remove(request_id)
//     }
//
//     fn insert(
//         &self,
//         request_id: RequestId,
//         sender: oneshot::Sender<Result<Bytes, RpcError>>,
//         protocol_id: ProtocolId,
//     ) {
//         let val = OpenRpcRequestState{
//             id: request_id,
//             sender,
//             protocol_id,
//         };
//         self.open_outbound_rpc.write().unwrap().insert(request_id, val);
//     }
// }

// type OutboundRpcMatcher = Arc<RwLock<BTreeMap<RequestId, OpenRpcRequestState>>>;
//
// fn NewOutboundRpcMatcher() -> OutboundRpcMatcher {
//     Arc::new(RwLock::new(BTreeMap::new()))
// }

pub struct NetworkEvents<TMessage> {
    network_source: NetworkSource, //::sync::mpsc::Receiver<ReceivedMessage>,
    done: bool,
    // this handle to outbound RPC catches replies and puts them on their oneshot channel
    // open_outbound_rpc: OutboundRpcMatcher,//Arc<RwLock<BTreeMap<RequestId, OpenRpcRequestState>>>,

    peer_senders: Arc<OutboundPeerConnections>,
    peers: HashMap<PeerNetworkId, PeerStub>,
    peers_generation: u32,

    // TMessage is the type we will deserialize to
    phantom: PhantomData<TMessage>,

    label: String,
}

impl<TMessage: Message + Unpin> NetworkEvents<TMessage> {
    pub fn new(
        network_source: NetworkSource,
        // open_outbound_rpc: OutboundRpcMatcher,
        peer_senders: Arc<OutboundPeerConnections>,
        label: &str,
    ) -> Self {
        Self {
            network_source,
            done: false,
            // open_outbound_rpc,//Arc::new(RwLock::new(BTreeMap::new())),
            peer_senders,
            peers: HashMap::new(),
            peers_generation: 0,
            phantom: Default::default(),
            label: label.to_string(),
        }
    }

    fn update_peers(&mut self) {
        if let Some((new_peers, new_generation)) = self.peer_senders.get_generational(self.peers_generation) {
            self.peers_generation = new_generation;
            self.peers.clear();
            self.peers.extend(new_peers);
        }
    }
}

async fn rpc_response_sender(
    mut receiver: oneshot::Receiver<Result<Bytes, RpcError>>,
    rpc_id: RequestId,
    // peer_network_id: PeerId,
    // network_sender: NetworkSender<TMessage>,
    peer_sender: tokio::sync::mpsc::Sender<NetworkMessage>
) {
    // TODO: reimplement timeout
    // info!("app_int rpc_respond {}", rpc_id);
    let bytes = match receiver.await {
        Ok(iresult) => match iresult{
            Ok(bytes) => {bytes}
            Err(err2) => {
                // TODO: counter
                info!("app_int rpc_respond rpc_err: {}", err2);
                return;
            }
        }
        Err(err) => {
            // TODO: counter
            info!("app_int rpc_respond cancelled: {}", err);
            return;
        }
    };
    let blen = bytes.len();
    let msg = NetworkMessage::RpcResponse(RpcResponse{
        request_id: rpc_id,
        priority: 0,
        raw_response: bytes.into(),
    });
    match peer_sender.send(msg).await {
        Ok(_) => {
            info!("app_int rpc_respond {} bytes OK", blen);
            // TODO: counter
        }
        Err(_) => {
            info!("app_int rpc_respond {} bytes ERR", blen);
            // TODO: counter
        }
    }
}

impl<TMessage: Message + Unpin> Stream for NetworkEvents<TMessage> {
    type Item = Event<TMessage>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            info!("app_int poll still DONE");
            return Poll::Ready(None);
        }
        // throw away up to 10 messages while looking for one to return
        let mself = self.get_mut();
        for _ in 1..10 {
            let msg = match Pin::new(&mut mself.network_source).poll_next(cx) {
                Poll::Ready(x) => match x {
                    Some(msg) => {
                        info!("app_int msg {}", msg.protocol_id_as_str());
                        msg
                    }
                    None => {
                        info!("app_int poll DONE");
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
                        info!("app_int err msg discarded");
                    }
                    NetworkMessage::RpcRequest(request) => {
                        // TODO: figure out a way to multi-core protocol_id.from_bytes()
                        info!("app_int rpc_req {} id={}, {}b", request.protocol_id, request.request_id, request.raw_request.len());
                        let app_msg = match request.protocol_id.from_bytes(request.raw_request.as_slice()) {
                            Ok(x) => { x },
                            Err(err) => {
                                mself.done = true;
                                // TODO network2: log error, count error, close connection
                                warn!("app_int rpc_req {} id={}; err {}, {} {} -> {}; from {:?}", request.protocol_id, request.request_id, err, mself.label, request.raw_request.encode_hex_upper::<String>(), std::any::type_name::<TMessage>(), &mself.network_source);
                                return Poll::Ready(None);
                            }
                        };
                        let rpc_id = request.request_id;
                        // setup responder oneshot channel
                        let (responder, response_reader) = oneshot::channel();//oneshot::Sender<Result<Bytes, RpcError>>
                        mself.update_peers();
                        let raw_sender = match mself.peers.get(&msg.sender) {
                            None => {
                                warn!("app_int rpc_req no peer {} id={}", request.protocol_id, request.request_id);
                                return Poll::Pending;
                            }
                            Some(peer_stub) => {
                                peer_stub.sender.clone()
                            }
                        };
                        // when this spawned task reads from the response oneshot channel it will send it to the network peer
                        // TODO network2: reimplement timeout/cleanup
                        Handle::current().spawn(rpc_response_sender(response_reader, rpc_id, raw_sender));
                        // TODO network2: add rpc-response matching at the application level here
                        return Poll::Ready(Some(Event::RpcRequest(
                            msg.sender, app_msg, request.protocol_id, responder)))
                    }
                    NetworkMessage::RpcResponse(response) => {
                        unreachable!("NetworkMessage::RpcResponse should not arrive in NetworkEvents because it is handled by Peer and reconnected with oneshot there");
                        // TODO network2: add rpc-response matching at the application level here
                        // let request_state = mself.open_outbound_rpc.remove(&response.request_id);
                        // let request_state = match request_state {
                        //     None => {
                        //         // timeout or garbage collection or something. drop response.
                        //         // TODO network2 log/count dropped response
                        //         // TODO: drop this one message, but go back to local loop
                        //         return Poll::Pending;
                        //     }
                        //     Some(x) => { x }
                        // };
                        // let app_msg = match request_state.protocol_id.from_bytes(response.raw_response.as_slice()) {
                        //     Ok(x) => { x },
                        //     Err(_) => {
                        //         mself.done = true;
                        //         // TODO network2: log error, count error, close connection
                        //         return Poll::Ready(None);
                        //     }
                        // };
                        // request_state.sender.send(Ok(response.raw_response.into()));
                        // // we processed a legit message, even if it didn't come back through the expected channel, yield
                        return Poll::Pending
                    }
                    NetworkMessage::DirectSendMsg(message) => {
                        info!("app_int dm");
                        // TODO: figure out a way to multi-core protocol_id.from_bytes()
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
// pub struct CachedPeerConnections {
//     peer_senders: Arc<OutboundPeerConnections>,
//
//     peers: RwLock<HashMap<PeerNetworkId, PeerStub>>,
//     peers_generation: AtomicU32,
// }
//
// impl CachedPeerConnections {
//     fn new(peer_senders: Arc<OutboundPeerConnections>) -> Self {
//         Self{
//             peer_senders,
//             peers: RwLock::new(HashMap::new()),
//             peers_generation: AtomicU32::new(0),
//         }
//     }
//
//     fn update_peers(&self) {
//         let cur_generation = self.peers_generation.load(Ordering::SeqCst);
//         if let Some((new_peers, new_generation)) = self.peer_senders.get_generational(cur_generation) {
//             let mut writer = self.peers.write().unwrap();
//             self.peers_generation.store(new_generation, Ordering::SeqCst);
//             writer.clear();
//             writer.extend(new_peers);
//             // writer.deref_mut().from_iter(new_peers);
//         }
//     }
// }
//
// impl Clone for CachedPeerConnections {
//     fn clone(&self) -> Self {
//         Self {
//             peer_senders: self.peer_senders.clone(),
//             peers: RwLock::new(HashMap::new()),
//             peers_generation: AtomicU32::new(0),
//         }
//     }
// }

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
#[derive(Debug)]
pub struct NetworkSender<TMessage> {
    // TODO: rebuild NetworkSender around single-level network::framework2
    // peer_mgr_reqs_tx: PeerManagerRequestSender,
    // connection_reqs_tx: ConnectionRequestSender,
    // TODO: we don't actually need a "NetworkSender" per network id; leftover structure from pre-2023 networking code
    network_id: NetworkId,
    peer_senders: Arc<OutboundPeerConnections>,

    peers: RwLock<HashMap<PeerNetworkId, PeerStub>>,
    peers_generation: AtomicU32,
    _marker: PhantomData<TMessage>,
}

/// Trait specifying the signature for `new()` `NetworkSender`s
pub trait NewNetworkSender {
    fn new(
        network_id: NetworkId,
        peer_senders: Arc<OutboundPeerConnections>,
    ) -> Self;
}

impl<TMessage> NewNetworkSender for NetworkSender<TMessage> {
    fn new(
        network_id: NetworkId,
        peer_senders: Arc<OutboundPeerConnections>,
    ) -> Self {
        Self {
            network_id,
            peer_senders,
            peers: RwLock::new(HashMap::new()),
            peers_generation: AtomicU32::new(0),
            _marker: PhantomData,
        }
    }
}

impl<TMessage> Clone for NetworkSender<TMessage> {
    fn clone(&self) -> Self {
        let reader = self.peers.read().unwrap();
        let peers_generation = self.peers_generation.load(Ordering::SeqCst);
        let peers = reader.clone();
        Self{
            network_id: self.network_id,
            peer_senders: self.peer_senders.clone(),
            peers: RwLock::new(peers),
            peers_generation: AtomicU32::new(peers_generation),
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

    fn update_peers(&self) {
        let cur_generation = self.peers_generation.load(Ordering::SeqCst);
        if let Some((new_peers, new_generation)) = self.peer_senders.get_generational(cur_generation) {
            let mut writer = self.peers.write().unwrap();
            self.peers_generation.store(new_generation, Ordering::SeqCst);
            writer.clear();
            writer.extend(new_peers);
            // writer.deref_mut().from_iter(new_peers);
        }
    }
}

impl<TMessage: Message> NetworkSender<TMessage> {
    fn peer_try_send<F>(&self, peer_network_id: &PeerNetworkId, msg_src: F) -> Result<(), NetworkError>
    where F: Fn() -> Result<NetworkMessage,NetworkError>
    {
        match self.peers.read() {
            Ok(peers) => {
                match peers.get(peer_network_id) {
                    None => {
                        Err(NetworkErrorKind::NotConnected.into())
                    }
                    Some(peer) => {
                        let msg = msg_src()?;
                        match peer.sender.try_send(msg) {
                            Ok(_) => {Ok(())}
                            Err(tse) => match &tse {
                                TrySendError::Full(_) => {
                                    // TODO: counter for send drop due to full
                                    Err(NetworkError::from(anyhow::Error::new(tse)))
                                }
                                TrySendError::Closed(_) => {
                                    // TODO: counter for send drop due to closed (other code should remove peer from collection)
                                    Err(NetworkError::from(anyhow::Error::new(tse)))
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // lock fail, wtf
                Err(NetworkError::from(NetworkErrorKind::IoError))
            }
        }
    }

    /// Send a message to a single recipient.
    pub fn send_to(
        &self,
        recipient: PeerId,
        protocol: ProtocolId,
        message: TMessage,
    ) -> Result<(), NetworkError> {
        let peer_network_id = PeerNetworkId::new(self.network_id, recipient);
        let msg_src = || -> Result<NetworkMessage,NetworkError> {
            // TODO: figure out a way to multi-core protocol_id.to_bytes()
            let mdata = protocol.to_bytes(&message)?.into();
            Ok(NetworkMessage::DirectSendMsg(DirectSendMsg {
                protocol_id: protocol,
                priority: 0,
                raw_msg: mdata,
            }))
        };
        self.update_peers();
        self.peer_try_send(&peer_network_id, msg_src)
    }

    /// Send a message to a many recipients.
    pub fn send_to_many(
        &self,
        recipients: impl Iterator<Item = PeerId>,
        protocol: ProtocolId,
        message: TMessage,
    ) -> Result<(), NetworkError> {
        // TODO: figure out a way to multi-core protocol_id.to_bytes()
        let mdata : Vec<u8> = protocol.to_bytes(&message)?.into();
        let msg_src = || -> Result<NetworkMessage,NetworkError> {
            Ok(NetworkMessage::DirectSendMsg(DirectSendMsg {
                protocol_id: protocol,
                priority: 0,
                raw_msg: mdata.clone(),
            }))
        };
        self.update_peers();
        let mut errs = vec![];
        for recipient in recipients {
            let peer_network_id = PeerNetworkId::new(self.network_id, recipient);
            match self.peer_try_send(&peer_network_id, msg_src) {
                Ok(_) => {}
                Err(xe) => {errs.push(xe)}
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            // return first error. TODO: return summary or concatenation of all errors
            for err in errs.into_iter() {
                return Err(err);
            }
            Ok(())
        }
    }

    /// Send a rpc request to a single recipient while handling
    /// serialization and deserialization of the request and response respectively.
    /// Assumes that the request and response both have the same message type.
    pub async fn send_rpc(
        &self,
        recipient: PeerId,
        protocol: ProtocolId,
        req_msg: TMessage,
        timeout: Duration,
    ) -> Result<TMessage, RpcError> {
        let deadline = tokio::time::Instant::now().add(timeout);
        let peer_network_id = PeerNetworkId::new(self.network_id, recipient);
        self.update_peers();
        // This holds a read-lock on the application's local cache of the peer map for a little while.
        // The big part is probably serialization in protocol.to_bytes().
        // peer.sender.try_send() should either accept or fail quickly.
        let receiver = match self.peers.read() {
            Ok(peers) => {
                match peers.get(&peer_network_id) {
                    None => {
                        return Err(RpcError::NotConnected(recipient));
                    }
                    Some(peer) => {
                        // TODO: figure out a way to multi-core protocol_id.to_bytes()
                        let mdata = protocol.to_bytes(&req_msg)?.into();
                        let request_id = peer.rpc_counter.fetch_add(1, Ordering::SeqCst);
                        let msg = NetworkMessage::RpcRequest(RpcRequest {
                            protocol_id: protocol,
                            request_id,
                            priority: 0,
                            raw_request: mdata,
                        });

                        let (sender, receiver) = oneshot::channel();
                        peer.open_outbound_rpc.insert(request_id, sender, protocol, deadline);
                        match peer.sender.try_send(msg) {
                            Ok(_) => {
                                receiver
                                // TODO: now we wait for rpc reply, connect to NetworkSource for this app
                                // Ok(())
                                // return Err(RpcError::TimedOut);
                            }
                            Err(tse) => match &tse {
                                TrySendError::Full(_) => {
                                    // TODO: counter for send drop due to full
                                    // TODO: we could wait, but we're holding a read lock on self.peers, and that would suck?
                                    return Err(RpcError::TooManyPending(1)); // TODO: look up the channel size and return that many pending?
                                }
                                TrySendError::Closed(_) => {
                                    // TODO: counter for send drop due to closed (other code should remove peer from collection)
                                    return Err(RpcError::NotConnected(recipient));
                                }
                            }
                        }
                    }
                }
            }
            Err(le) => {
                // lock fail, wtf
                // TODO: better error for lock fail?
                return Err(RpcError::TimedOut);
            }
        };
        let sub_timeout = match deadline.checked_duration_since(tokio::time::Instant::now()) {
            None => {
                return Err(RpcError::TimedOut);
            }
            Some(sub) => {sub}
        };
        match tokio::time::timeout(sub_timeout, receiver).await {
            Ok(receiver_result) => match receiver_result {
                Ok(content_result) => match content_result {
                    Ok(bytes) => {
                        let wat = protocol.from_bytes(bytes.as_ref())?;
                        info!("app_int rpc reply good {} bytes", bytes.len());
                        return Ok(wat);
                    }
                    Err(err) => {
                        // TODO: counter
                        warn!("app_int rpc reply err: {}", err);
                        return Err(err);
                    }
                }
                Err(_cancelled) => {
                    // TODO: counter
                    warn!("app_int rpc reply cancelled");
                    return Err(RpcError::UnexpectedResponseChannelCancel);
                }
            }
            Err(_timeout) => {
                // TODO: counter
                warn!("app_int rpc reply timeout");
                return Err(RpcError::TimedOut)
            }
        }
    }

    /// Send a message to a single recipient.
    pub fn rpc_reply(
        &self,
        recipient: PeerId,
        protocol: ProtocolId,
        request_id: RequestId,
        message: TMessage,
    ) -> Result<(), NetworkError> {
        let peer_network_id = PeerNetworkId::new(self.network_id, recipient);
        let msg_src = || -> Result<NetworkMessage,NetworkError> {
            let mdata = protocol.to_bytes(&message)?.into();
            Ok(NetworkMessage::RpcResponse(RpcResponse {
                request_id: 0,
                priority: 0,
                raw_response: mdata,
            }))
        };
        self.update_peers();
        self.peer_try_send(&peer_network_id, msg_src)
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

#[derive(Debug)]
enum NetworkSourceUnion {
    SingleSource(tokio_stream::wrappers::ReceiverStream<ReceivedMessage>),
    ManySource(futures::stream::SelectAll<ReceiverStream<ReceivedMessage>>),
}

#[derive(Debug)]
pub struct NetworkSource {
    source: NetworkSourceUnion,
}

/// NetworkSource implements Stream<ReceivedMessage> and is all messages routed to this application from all peers
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
}

#[derive(Clone, Debug)]
pub struct PeerStub {
    /// channel to Peer's write thread
    /// TODO: also add high-priority channel
    pub sender: tokio::sync::mpsc::Sender<NetworkMessage>,
    pub rpc_counter: Arc<AtomicU32>,
    open_outbound_rpc: OutboundRpcMatcher,
}

impl PeerStub {
    pub fn new(sender: tokio::sync::mpsc::Sender<NetworkMessage>, open_outbound_rpc: OutboundRpcMatcher) -> Self {
        Self {
            sender,
            rpc_counter: Arc::new(AtomicU32::new(0)),
            open_outbound_rpc,
        }
    }
}

/// Container for map of PeerNetworkId and associated outbound channels.
/// Generational fetch for fast no-change path. Local map copy can then operate lock free.
#[derive(Debug)]
pub struct OutboundPeerConnections {
    peer_connections: RwLock<HashMap<PeerNetworkId, PeerStub>>,
    generation: AtomicU32,
}

impl OutboundPeerConnections {
    pub fn new() -> Self {
        Self{
            peer_connections: RwLock::new(HashMap::new()),
            generation: AtomicU32::new(0),
        }
    }

    /// pass in a generation number, if it is stale return new peer map and current generation, otherwise None
    pub fn get_generational(&self, generation: u32) -> Option<(HashMap<PeerNetworkId,PeerStub>, u32)> {
        let generation_test = self.generation.load(Ordering::SeqCst);
        if generation == generation_test {
            return None;
        }
        let read = self.peer_connections.read().unwrap();
        let generation_actual = self.generation.load(Ordering::SeqCst);
        let out = read.clone();
        Some((out, generation_actual))
    }

    /// set a (PeerNetworkId, PeerStub) pair
    /// return new generation counter
    pub fn insert(&self, peer_network_id: PeerNetworkId, peer: PeerStub) -> u32 {
        let mut write = self.peer_connections.write().unwrap();
        write.insert(peer_network_id, peer);
        self.generation.fetch_add(1 , Ordering::SeqCst)
    }

    /// remove a PeerNetworkId entry
    /// return new generation counter
    pub fn remove(&self, peer_network_id: &PeerNetworkId) -> u32 {
        let mut write = self.peer_connections.write().unwrap();
        write.remove(peer_network_id);
        self.generation.fetch_add(1 , Ordering::SeqCst)
    }
}
