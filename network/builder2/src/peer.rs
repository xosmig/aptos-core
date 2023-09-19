// Copyright © Aptos Foundation

use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::mpsc::Receiver;
use aptos_network2::protocols::wire::messaging::v1::{MultiplexMessage, MultiplexMessageSink, MultiplexMessageStream, NetworkMessage};
use crate::ApplicationCollector;
use futures::io::{AsyncRead,AsyncReadExt,AsyncWrite};
use futures::StreamExt;
use futures::SinkExt;
use futures::stream::Fuse;
use tokio::sync::mpsc::error::{SendError, TryRecvError};
use aptos_config::network_id::PeerNetworkId;
use aptos_logger::{error, info, warn};
use aptos_network2::protocols::network::{PeerStub, ReceivedMessage};
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
pub const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024; /* 4 MiB large messages will be chunked into multiple frames and streamed */
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024; /* 64 MiB */

pub fn start_peer<TSocket>(
    socket: TSocket,
    to_send: Receiver<NetworkMessage>,
    apps: Arc<ApplicationCollector>,
    handle: Handle,
    remote_peer_network_id: PeerNetworkId,
)
where
    TSocket: aptos_network2::transport::TSocket
{
    // let max_frame_size = 4 * 1024 * 1024; // TODO: make configurable?
    // let max_message_size = 64 * 1024 * 1024; // TODO: make configurable?
    let (read_socket, write_socket) = socket.split();
    let reader =
        MultiplexMessageStream::new(read_socket, MAX_FRAME_SIZE).fuse();
    let writer = MultiplexMessageSink::new(write_socket, MAX_FRAME_SIZE);
    handle.spawn(writer_task(to_send, writer));
    handle.spawn(reader_task(reader, apps, remote_peer_network_id));
}

struct WriterContext<WriteThing: AsyncWrite + Unpin + Send> {
    stream_request_id : u32,
    large_message: Option<Vec<u8>>,
    large_fragment_id: u8,
    send_large: bool,
    next_msg: Option<NetworkMessage>,
    max_frame_size: usize,

    to_send: Receiver<NetworkMessage>,
    writer: MultiplexMessageSink<WriteThing>,
}

impl<WriteThing: AsyncWrite + Unpin + Send> WriterContext<WriteThing> {
    fn new(
        to_send: Receiver<NetworkMessage>,
        writer: MultiplexMessageSink<WriteThing>,
    ) -> Self {
        Self {
            stream_request_id: 0,
            large_message: None,
            large_fragment_id: 0,
            send_large: false,
            next_msg: None,
            max_frame_size: MAX_FRAME_SIZE,
            to_send,
            writer,
        }
    }

    fn next_large(&mut self) -> MultiplexMessage {
        let mut blob = self.large_message.take().unwrap();
        if blob.len() > MAX_FRAME_SIZE {
            let rest = blob.split_off(MAX_FRAME_SIZE);
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
        let mut num_fragments = msg.data_len() / MAX_FRAME_SIZE;
        let mut msg = msg;
        while num_fragments * MAX_FRAME_SIZE < msg.data_len() {
            num_fragments = num_fragments + 1;
        }
        if num_fragments > 0x0ff {
            panic!("huge message cannot be fragmented {:?} > 255 * {:?}", msg.data_len(), MAX_FRAME_SIZE);
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
                if self.send_large || self.next_msg.is_some() {
                    self.next_large()
                } else {
                    match self.to_send.try_recv() {
                        Ok(msg) => {
                            if msg.data_len() > MAX_FRAME_SIZE {
                                // finish prior large message before starting a new large message
                                self.next_msg = Some(msg);
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
            } else if self.next_msg.is_some() {
                let msg = self.next_msg.take().unwrap();
                self.start_large(msg)
            } else {
                match self.to_send.recv().await {
                    None => {
                        info!("peer writer source closed");
                        break;
                    }
                    Some(msg) => {
                        if msg.data_len() > MAX_FRAME_SIZE {
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
) {
    let wt = WriterContext::new(to_send, writer);
    wt.run().await;
}

// async fn writer_taskX(
//     mut to_send: Receiver<NetworkMessage>,
//     mut writer: MultiplexMessageSink<impl AsyncWrite + Unpin + Send + 'static>,
// ) {
//     let mut stream_request_id : u32 = 0;
//     let mut large_message : Option<Vec<u8>> = None;
//     let mut large_fragment_id : u8 = 0;
//     let mut send_large = false;
//     let mut next_msg : Option<NetworkMessage> = None;
//     loop {
//         let mm = if large_message.is_some() {
//             if send_large || next_msg.is_some() {
//                 let mut blob = large_message.take().unwrap();
//                 if blob.len() > MAX_FRAME_SIZE {
//                     let rest = blob.split_off(MAX_FRAME_SIZE);
//                     large_message = Some(rest);
//                 }
//                 large_fragment_id = large_fragment_id + 1;
//                 send_large = false;
//                 StreamMessage::Fragment(StreamFragment {
//                     stream_request_id,
//                     fragment_id: large_fragment_id,
//                     raw_data: blob,
//                 })
//             } else {
//                 match to_send.try_recv() {
//                     Ok(msg) => {
//                         if msg.data_len() > MAX_FRAME_SIZE {
//                             // finish prior large message before starting a new large message
//                             next_msg = Some(msg);
//                         } else {
//                             send_large = true;
//                             MultiplexMessage::Message(msg)
//                         }
//                     }
//                     Err(err) => match err {
//                         TryRecvError::Empty => {
//
//                         }
//                         TryRecvError::Disconnected => {}
//                     }
//                 }
//             }
//         } else {
//             match to_send.recv().await {
//                 None => {
//                     info!("peer writer source closed");
//                     break;
//                 }
//                 Some(msg) => {
//                     if msg.data_len() > MAX_FRAME_SIZE {
//                         // start stream
//                         stream_request_id = stream_request_id + 1;
//                         send_large = false;
//                         large_fragment_id = 0;
//                         let mut num_fragments = msg.data_len() / MAX_FRAME_SIZE;
//                         while num_fragments * MAX_FRAME_SIZE < msg.data_len() {
//                             num_fragments = num_fragments + 1;
//                         }
//                         if num_fragments > 0x0ff {
//                             error!("huge message cannot be fragmented {:?} > 255 * {:?}", msg.data_len(), MAX_FRAME_SIZE); // panic()?
//                             break;
//                         }
//                         let num_fragments = num_fragments as u8;
//                         let rest = match &mut msg {
//                             NetworkMessage::Error(_) => {
//                                 unreachable!("NetworkMessage::Error should always fit in a single frame")
//                             },
//                             NetworkMessage::RpcRequest(request) => {
//                                 request.raw_request.split_off(self.max_frame_size)
//                             },
//                             NetworkMessage::RpcResponse(response) => {
//                                 response.raw_response.split_off(self.max_frame_size)
//                             },
//                             NetworkMessage::DirectSendMsg(message) => {
//                                 message.raw_msg.split_off(self.max_frame_size)
//                             },
//                         };
//                         large_message = Some(rest);
//                         MultiplexMessage::Stream(StreamMessage::Header(StreamHeader {
//                             stream_request_id,
//                             num_fragments,
//                             msg,
//                         }))
//                     } else {
//                         MultiplexMessage::Message(msg)
//                     }
//                 }
//             }
//         };
//         // while let Some(msg) = to_send.recv().await {
//         // TODO: rebuild large message chunking
//         // let mm = MultiplexMessage::Message(msg);
//         match writer.send(&mm).await {
//             Ok(_) => {
//                 // TODO: counter msg sent, msg size sent
//             }
//             Err(err) => {
//                 // TODO: counter net write err
//                 warn!("error sending message to peer: {:?}", err);
//             }
//         }
//     }
//     info!("peer writer closing"); // TODO: cause the reader to close?
// }

async fn reader_task(
    mut reader: Fuse<MultiplexMessageStream<impl AsyncRead + Unpin>>,
    apps: Arc<ApplicationCollector>,
    remote_peer_network_id: PeerNetworkId,
) {
    while let Some(msg) = reader.next().await {
        let msg = match msg {
            Ok(msg) => {msg}
            Err(err) => {
                // TODO: counter
                warn!("read error {:?}", err);
                continue;
            }
        };
        match msg {
            MultiplexMessage::Message(msg) => {
                match &msg {
                    NetworkMessage::Error(errm) => {
                        // TODO: counter
                        warn!("got error message: {:?}", errm)
                    }
                    NetworkMessage::RpcRequest(request) => {
                        match apps.apps.get(&request.protocol_id) {
                            None => {
                                // TODO: counter
                                warn!("got message for protocol {:?} we don't handle", request.protocol_id);
                                // TODO: drop connection
                            }
                            Some(app) => {
                                match app.sender.send(ReceivedMessage{ message: msg, sender: remote_peer_network_id }).await {
                                    Ok(_) => {
                                        // TODO: counter
                                    }
                                    Err(err) => {
                                        // TODO: counter
                                    }
                                }
                            }
                        }
                    }
                    NetworkMessage::RpcResponse(_) => {}
                    NetworkMessage::DirectSendMsg(_) => {}
                }
            }
            MultiplexMessage::Stream(_) => {}
        }
        //message: Result<MultiplexMessage, ReadError>,
    }
    info!("peer reader finished"); // TODO: cause the writer to close?
}
