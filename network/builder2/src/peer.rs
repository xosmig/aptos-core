// Copyright © Aptos Foundation

use std::sync::Arc;
use futures::channel::oneshot;
use tokio::runtime::Handle;
use tokio::sync::mpsc::Receiver;
use aptos_network2::protocols::wire::messaging::v1::{MultiplexMessage, MultiplexMessageSink, MultiplexMessageStream, NetworkMessage};
use bytes::Bytes;
use crate::{ApplicationCollector, ApplicationConnections};
use futures::io::{AsyncRead,AsyncReadExt,AsyncWrite};
use futures::StreamExt;
use futures::SinkExt;
use futures::stream::Fuse;
use tokio::sync::mpsc::error::{SendError, TryRecvError};
use aptos_config::config::NetworkConfig;
use aptos_config::network_id::PeerNetworkId;
use aptos_logger::{error, info, warn};
use aptos_network2::application::interface::{OpenRpcRequestState, OutboundRpcMatcher};
use aptos_network2::ProtocolId;
use aptos_network2::protocols::network::{PeerStub, ReceivedMessage, RpcError};
use aptos_network2::protocols::stream::{StreamFragment, StreamHeader, StreamMessage};

//
// /// Peer holds what is needed by the write thread
// pub struct Peer<TSocket>
// where
// TSocket: aptos_network2::transport::TSocket
// {
//     // socket: TSocket,
//     /// messages from apps to write out to socket
//     to_send: Receiver<NetworkMessage>,
//     apps: Arc<ApplicationCollector>,
// }
//
// impl<TSocket> Peer<TSocket>
//     where
//         TSocket: aptos_network2::transport::TSocket
// {
//     pub fn new(
//         socket: TSocket,
//         to_send: Receiver<NetworkMessage>,
//         apps: Arc<ApplicationCollector>,
//     ) -> Self {
//         Self{
//             socket,
//             to_send,
//             apps,
//         }
//     }
//
//     pub fn start(handle: Handle) {
//         let (reader, writer) = tokio:io::split(self.socket);
//     }
// }

// TODO: use values from net config
// pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024; /* 4 MiB large messages will be chunked into multiple frames and streamed */
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024; /* 64 MiB */

pub fn start_peer<TSocket>(
    config: &NetworkConfig,
    socket: TSocket,
    to_send: Receiver<NetworkMessage>,
    apps: Arc<ApplicationCollector>,
    handle: Handle,
    remote_peer_network_id: PeerNetworkId,
    open_outbound_rpc: OutboundRpcMatcher,
)
where
    TSocket: aptos_network2::transport::TSocket
{
    // let max_message_size = 64 * 1024 * 1024; // TODO: make configurable?
    let max_frame_size = config.max_frame_size;
    let (read_socket, write_socket) = socket.split();
    let reader =
        MultiplexMessageStream::new(read_socket, max_frame_size).fuse();
    let writer = MultiplexMessageSink::new(write_socket, max_frame_size);
    handle.spawn(writer_task(to_send, writer, max_frame_size));
    handle.spawn(reader_task(reader, apps, remote_peer_network_id, open_outbound_rpc, handle.clone()));
}

/// state needed in writer_task()
struct WriterContext<WriteThing: AsyncWrite + Unpin + Send> {
    /// increment for each new fragment stream
    stream_request_id : u32,
    /// remaining payload bytes of curretnly fragmenting large message
    large_message: Option<Vec<u8>>,
    /// index into chain of fragments
    large_fragment_id: u8,
    /// toggle to send normal msg or send fragment of large message
    send_large: bool,
    /// if we have a large message in flight and another arrives, stash it here
    next_large_msg: Option<NetworkMessage>,
    /// TODO: pull this from node config
    max_frame_size: usize,

    /// messages from apps to send to the peer
    to_send: Receiver<NetworkMessage>,
    /// encoder wrapper around socket write half
    writer: MultiplexMessageSink<WriteThing>,
}

impl<WriteThing: AsyncWrite + Unpin + Send> WriterContext<WriteThing> {
    fn new(
        to_send: Receiver<NetworkMessage>,
        writer: MultiplexMessageSink<WriteThing>,
        max_frame_size: usize,
    ) -> Self {
        Self {
            stream_request_id: 0,
            large_message: None,
            large_fragment_id: 0,
            send_large: false,
            next_large_msg: None,
            max_frame_size,
            to_send,
            writer,
        }
    }

    /// send a next chunk from a currently fragmenting large message
    fn next_large(&mut self) -> MultiplexMessage {
        let mut blob = self.large_message.take().unwrap();
        if blob.len() > self.max_frame_size {
            let rest = blob.split_off(self.max_frame_size);
            self.large_message = Some(rest);
        }
        self.large_fragment_id = self.large_fragment_id + 1;
        self.send_large = false;
        MultiplexMessage::Stream(StreamMessage::Fragment(StreamFragment {
            request_id: self.stream_request_id,
            fragment_id: self.large_fragment_id,
            raw_data: blob,
        }))
    }

    fn start_large(&mut self, msg: NetworkMessage) -> MultiplexMessage {
        self.stream_request_id = self.stream_request_id + 1;
        self.send_large = false;
        self.large_fragment_id = 0;
        let mut num_fragments = msg.data_len() / self.max_frame_size;
        let mut msg = msg;
        while num_fragments * self.max_frame_size < msg.data_len() {
            num_fragments = num_fragments + 1;
        }
        if num_fragments > 0x0ff {
            panic!("huge message cannot be fragmented {:?} > 255 * {:?}", msg.data_len(), self.max_frame_size);
        }
        let num_fragments = num_fragments as u8;
        let rest = match &mut msg {
            NetworkMessage::Error(_) => {
                unreachable!("NetworkMessage::Error should always fit in a single frame")
            },
            NetworkMessage::RpcRequest(request) => {
                request.raw_request.split_off(self.max_frame_size)
            },
            NetworkMessage::RpcResponse(response) => {
                response.raw_response.split_off(self.max_frame_size)
            },
            NetworkMessage::DirectSendMsg(message) => {
                message.raw_msg.split_off(self.max_frame_size)
            },
        };
        self.large_message = Some(rest);
        MultiplexMessage::Stream(StreamMessage::Header(StreamHeader {
            request_id: self.stream_request_id,
            num_fragments,
            message: msg,
        }))
    }

    async fn run(mut self) {
        loop {
            let mm = if self.large_message.is_some() {
                if self.send_large || self.next_large_msg.is_some() {
                    self.next_large()
                } else {
                    match self.to_send.try_recv() {
                        Ok(msg) => {
                            if msg.data_len() > self.max_frame_size {
                                // finish prior large message before starting a new large message
                                self.next_large_msg = Some(msg);
                                self.next_large()
                            } else {
                                // send small message now, large chunk next
                                self.send_large = true;
                                MultiplexMessage::Message(msg)
                            }
                        }
                        Err(err) => match err {
                            TryRecvError::Empty => {
                                // ok, no next small msg, continue with chunks of large message
                                self.next_large()
                            }
                            TryRecvError::Disconnected => {
                                info!("peer writer source closed");
                                break
                            }
                        }
                    }
                }
            } else if self.next_large_msg.is_some() {
                let msg = self.next_large_msg.take().unwrap();
                self.start_large(msg)
            } else {
                match self.to_send.recv().await {
                    None => {
                        info!("peer writer source closed");
                        break;
                    }
                    Some(msg) => {
                        if msg.data_len() > self.max_frame_size {
                            // start stream
                            self.start_large(msg)
                        } else {
                            MultiplexMessage::Message(msg)
                        }
                    }
                }
            };
            // while let Some(msg) = to_send.recv().await {
            // TODO: rebuild large message chunking
            // let mm = MultiplexMessage::Message(msg);
            match self.writer.send(&mm).await {
                Ok(_) => {
                    // TODO: counter msg sent, msg size sent
                }
                Err(err) => {
                    // TODO: counter net write err
                    warn!("error sending message to peer: {:?}", err);
                }
            }
        }
        info!("peer writer closing"); // TODO: cause the reader to close?
    }

    fn split_message(&self, msg: &mut NetworkMessage) -> Vec<u8> {
        match msg {
            NetworkMessage::Error(_) => {
                unreachable!("NetworkMessage::Error should always fit in a single frame")
            },
            NetworkMessage::RpcRequest(request) => {
                request.raw_request.split_off(self.max_frame_size)
            },
            NetworkMessage::RpcResponse(response) => {
                response.raw_response.split_off(self.max_frame_size)
            },
            NetworkMessage::DirectSendMsg(message) => {
                message.raw_msg.split_off(self.max_frame_size)
            },
        }
    }
}

async fn writer_task(
    mut to_send: Receiver<NetworkMessage>,
    mut writer: MultiplexMessageSink<impl AsyncWrite + Unpin + Send + 'static>,
    max_frame_size: usize,
) {
    let wt = WriterContext::new(to_send, writer, max_frame_size);
    wt.run().await;
}

async fn complete_rpc(sender: oneshot::Sender<Result<Bytes,RpcError>>, nmsg: NetworkMessage) {//: Vec<u8>) {
    if let NetworkMessage::RpcResponse(response) = nmsg {
        let blob = response.raw_response;
        match sender.send(Ok(blob.into())) {
            Ok(_) => {
                // TODO: counter rpc completion to app
            }
            Err(err) => {
                // TODO: counter rpc completion dropped at app
                warn!("rpc completion dropped at app")
            }
        }
    }
}

struct ReaderContext<ReadThing: AsyncRead + Unpin> {
    reader: Fuse<MultiplexMessageStream<ReadThing>>,
    apps: Arc<ApplicationCollector>,
    remote_peer_network_id: PeerNetworkId,
    open_outbound_rpc: OutboundRpcMatcher,
    handle: Handle,
}

impl<ReadThing: AsyncRead + Unpin> ReaderContext<ReadThing> {
    fn new(
        reader: Fuse<MultiplexMessageStream<ReadThing>>,
        apps: Arc<ApplicationCollector>,
        remote_peer_network_id: PeerNetworkId,
        open_outbound_rpc: OutboundRpcMatcher,
        handle: Handle,
    ) -> Self {
        Self {
            reader,
            apps,
            remote_peer_network_id,
            open_outbound_rpc,
            handle,
        }
    }

    async fn forward(&self, protocol_id: ProtocolId, nmsg: NetworkMessage) {
        match self.apps.apps.get(&protocol_id) {
            None => {
                // TODO: counter
                warn!("got rpc req for protocol {:?} we don't handle", protocol_id);
                // TODO: drop connection
            }
            Some(app) => {
                match app.sender.send(ReceivedMessage{ message: nmsg, sender: self.remote_peer_network_id }).await {
                    Ok(_) => {
                        // TODO: counter
                    }
                    Err(err) => {
                        // TODO: counter
                        error!("app channel protocol_id={:?} err={:?}", protocol_id, err);
                    }
                }
            }
        }
    }

    async fn handle_message(&self, nmsg: NetworkMessage) {
        match &nmsg {
            NetworkMessage::Error(errm) => {
                // TODO: counter
                warn!("got error message: {:?}", errm)
            }
            NetworkMessage::RpcRequest(request) => {
                let protocol_id = request.protocol_id;
                self.forward(protocol_id, nmsg);
            }
            NetworkMessage::RpcResponse(response) => {
                match self.open_outbound_rpc.remove(&response.request_id) {
                    None => {
                        // TODO: counter rpc response dropped, no receiver
                    }
                    Some(rpc_state) => {
                        self.handle.spawn(complete_rpc(rpc_state.sender, nmsg));//response.raw_response));
                    }
                }
            }
            NetworkMessage::DirectSendMsg(message) => {
                let protocol_id = message.protocol_id;
                self.forward(protocol_id, nmsg);
            }
        }
   }

    async fn run(mut self) {
        let mut current_stream_id : u32 = 0;
        let mut large_message : Option<NetworkMessage> = None;
        let mut fragment_index : u8 = 0;
        let mut num_fragments : u8 = 0;
        while let Some(msg) = self.reader.next().await {
            let msg = match msg {
                Ok(msg) => {msg}
                Err(err) => {
                    // TODO: counter
                    warn!("read error {:?}", err);
                    continue;
                }
            };
            match msg {
                MultiplexMessage::Message(nmsg) => {
                    self.handle_message(nmsg);
                }
                MultiplexMessage::Stream(fragment) => match fragment {
                    StreamMessage::Header(head) => {
                        if num_fragments != fragment_index {
                            warn!("fragment index = {:?} of {:?} total fragments with new stream header", fragment_index, num_fragments);
                        }
                        current_stream_id = head.request_id;
                        num_fragments = head.num_fragments;
                        large_message = Some(head.message);
                        fragment_index = 1;
                    }
                    StreamMessage::Fragment(more) => {
                        if more.request_id != current_stream_id {
                            warn!("got stream request_id={:?} while {:?} was in progress", more.request_id, current_stream_id);
                            // TODO: counter? disconnect from peer?
                            num_fragments = 0;
                            fragment_index = 0;
                            continue;
                        }
                        if more.fragment_id != fragment_index {
                            warn!("got fragment_id {:?}, expected {:?}", more.fragment_id, fragment_index);
                            // TODO: counter? disconnect from peer?
                            num_fragments = 0;
                            fragment_index = 0;
                            continue;
                        }
                        match large_message.as_mut() {
                            None => {
                                warn!("got fragment without header");
                                continue;
                            }
                            Some(lm) => match lm {
                                NetworkMessage::Error(_) => {
                                    unreachable!("stream fragment should never be NetworkMessage::Error")
                                }
                                NetworkMessage::RpcRequest(request) => {
                                    request.raw_request.extend_from_slice(more.raw_data.as_slice());
                                }
                                NetworkMessage::RpcResponse(response) => {
                                    response.raw_response.extend_from_slice(more.raw_data.as_slice());
                                }
                                NetworkMessage::DirectSendMsg(message) => {
                                    message.raw_msg.extend_from_slice(more.raw_data.as_slice());
                                }
                            }
                        }
                        fragment_index += 1;
                        if fragment_index == num_fragments {
                            self.handle_message(large_message.take().unwrap());
                        }
                    }
                }
            }
        }
    }
}

async fn reader_task(
    mut reader: Fuse<MultiplexMessageStream<impl AsyncRead + Unpin>>,
    apps: Arc<ApplicationCollector>,
    remote_peer_network_id: PeerNetworkId,
    open_outbound_rpc: OutboundRpcMatcher,
    handle: Handle,
) {
    let rc = ReaderContext::new(reader, apps, remote_peer_network_id, open_outbound_rpc, handle);
    rc.run().await;
    info!("peer reader finished"); // TODO: cause the writer to close?
}
