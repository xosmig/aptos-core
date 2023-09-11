// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use std::io::Chain;
use std::marker::PhantomData;
use std::sync::Arc;
use futures::{AsyncRead, AsyncWrite};
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use aptos_config::config::{HANDSHAKE_VERSION, NetworkConfig, RoleType};
use aptos_config::network_id::{NetworkContext, PeerNetworkId};
use aptos_crypto::x25519;
use aptos_event_notifications::{DbBackedOnChainConfig,EventSubscriptionService};
use aptos_logger::error;
#[cfg(any(test, feature = "testing", feature = "fuzzing"))]
use aptos_netcore::transport::memory::MemoryTransport;
use aptos_netcore::transport::tcp::{TCPBufferCfg, TcpSocket, TcpTransport};
use aptos_netcore::transport::Transport;
// use aptos_network_discovery::DiscoveryChangeListener;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use aptos_network2::application::storage::PeersAndMetadata;
use aptos_network2::noise::stream::NoiseStream;
use aptos_network2::protocols::wire::handshake::v1::{ProtocolId, ProtocolIdSet};
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;
use aptos_network2::protocols::network::ReceivedMessage;
use aptos_network2::transport::{APTOS_TCP_TRANSPORT, AptosNetTransport, Connection};
use aptos_types::network_address::Protocol;

#[derive(Debug, PartialEq, PartialOrd)]
enum State {
    CREATED,
    BUILT,
    STARTED,
}


/// Inbound and Outbound connections are always secured with NoiseIK.  The dialer
/// will always verify the listener.
#[derive(Debug)]
pub enum AuthenticationMode {
    /// Inbound connections will first be checked against the known peers set, and
    /// if the `PeerId` is known it will be authenticated against it's `PublicKey`
    /// Otherwise, the incoming connections will be allowed through in the common
    /// pool of unknown peers.
    MaybeMutual(x25519::PrivateKey),
    /// Both dialer and listener will verify public keys of each other in the
    /// handshake.
    Mutual(x25519::PrivateKey),
}

/// Build Network module with custom configuration values.
/// Methods can be chained in order to set the configuration values.
/// MempoolNetworkHandler and ConsensusNetworkHandler are constructed by calling
/// [`NetworkBuilder::build`].  New instances of `NetworkBuilder` are obtained
/// via [`NetworkBuilder::create`].
pub struct NetworkBuilder {
    state: State,
    executor: Option<Handle>,
    time_service: TimeService,
    network_context: NetworkContext,
    chain_id: ChainId,
    config: NetworkConfig,
    // discovery_listeners: Option<Vec<DiscoveryChangeListener<DbBackedOnChainConfig>>>, // TODO network2: re-build known-peer-directory
    // connectivity_manager_builder: Option<ConnectivityManagerBuilder>, // TODO network2: re-enable connectivity manager
    // health_checker_builder: Option<HealthCheckerBuilder>,
    // peer_manager_builder: PeerManagerBuilder,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: Arc<ApplicationCollector>,
}

impl NetworkBuilder {
    /// Create a new NetworkBuilder based on the provided configuration.
    pub fn create(
        chain_id: ChainId,
        role: RoleType,
        config: &NetworkConfig,
        time_service: TimeService,
        mut reconfig_subscription_service: Option<&mut EventSubscriptionService>,
        peers_and_metadata: Arc<PeersAndMetadata>,
    ) -> NetworkBuilder {
        let peer_id = config.peer_id();
        // let identity_key = config.identity_key();
        // let pubkey = identity_key.public_key();
        let network_context = NetworkContext::new(role, config.network_id, peer_id);
        // let authentication_mode = if config.mutual_authentication {
        //     AuthenticationMode::Mutual(identity_key)
        // } else {
        //     AuthenticationMode::MaybeMutual(identity_key)
        // };
        NetworkBuilder{
            state: State::CREATED,
            executor: None,
            time_service,
            network_context,
            chain_id,
            config: config.clone(),
            // discovery_listeners: None,
            // connectivity_manager_builder: None,
            peers_and_metadata,
            apps: Arc::new(ApplicationCollector::new()), // temporary empty app set
        }
    }

    pub fn set_apps(&mut self, apps: Arc<ApplicationCollector>) {
        self.apps = apps;
    }

    pub fn active_protocol_ids(&self) -> ProtocolIdSet {
        let mut out = ProtocolIdSet::empty();
        for (protocol_id, _) in self.apps.apps.iter() {
            out.insert(*protocol_id);
        }
        out
    }

    pub fn deliver_message(&self, protocol_id: &ProtocolId, msg: ReceivedMessage) {
        match self.apps.apps.get(protocol_id) {
            None => {
                // TODO network2: peer sent to a ProtocolId we don't process. disconnect. inc a counter
            }
            Some(connections) => {
                match connections.sender.try_send(msg) {
                    Ok(_) => {
                        // TODO network2: inc per-protocolid counter
                        // TODO network2: measure time-in-queue for some messages?
                    }
                    Err(err) => match err {
                        TrySendError::Full(_) => {} // TODO network2: inc per-protocolid drop counter
                        TrySendError::Closed(_) => {
                            error!("channel to app for ProtocolId {:?} is closed", protocol_id);
                        }
                    }
                }
            }
        }
    }

    pub fn build(&mut self, handle: Handle) {
        if self.state != State::CREATED {
            panic!("NetworkBuilder.build but not in state CREATED");
        }
        self.state = State::BUILT;
    }

    fn get_tcp_buffers_cfg(&self) -> TCPBufferCfg {
        TCPBufferCfg::new_configs(
            self.config.inbound_rx_buffer_size_bytes,
            self.config.inbound_tx_buffer_size_bytes,
            self.config.outbound_rx_buffer_size_bytes,
            self.config.outbound_tx_buffer_size_bytes,
        )
    }

    fn build_transport(&mut self) -> TransportPeerManager {
        let listen_parts = self.config.listen_address.as_slice();
        let key = self.config.identity_key();
        // let trusted_peers = self.peers_and_metadata.get_trusted_peers(&self.config.network_id).unwrap();
        let mutual_auth = self.config.mutual_authentication;
        let protos = self.active_protocol_ids();
        let enable_proxy_protocol = self.config.enable_proxy_protocol;
        match listen_parts[0] {
            Protocol::Ip4(_) | Protocol::Ip6(_) => {
                // match listen_parts[1]
                // Protocol::Tcp(_) => {
                let mut aptos_tcp_transport = APTOS_TCP_TRANSPORT.clone();
                let tcp_cfg = self.get_tcp_buffers_cfg();
                aptos_tcp_transport.set_tcp_buffers(&tcp_cfg);
                let ant = AptosNetTransport::<TcpTransport>::new(
                    aptos_tcp_transport,
                    self.network_context,
                    self.time_service.clone(),
                    key,
                    // auth_mode,
                    self.peers_and_metadata.clone(),
                    mutual_auth,
                    HANDSHAKE_VERSION,
                    self.chain_id,
                    protos,
                    enable_proxy_protocol,
                );
                let pm = PeerManager::new(ant);
                TransportPeerManager::Tcp(pm)
            }
            #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
            Protocol::Memory(_) => {
                TransportPeerManager::Memory(PeerManager::new(AptosNetTransport::new(
                    MemoryTransport,
                    self.network_context,
                    self.time_service.clone(),
                    key,
                    // auth_mode,
                    self.peers_and_metadata.clone(),
                    mutual_auth,
                    HANDSHAKE_VERSION,
                    self.chain_id,
                    protos,
                    enable_proxy_protocol,
                )))
            }
            _ => {
                panic!("cannot listen on address {:?}", self.config.listen_address);
            }
        }
    }

    pub fn start(&mut self) {
        // TODO network2 start the built network
        if self.state != State::BUILT {
            panic!("NetworkBuilder.build but not in state BUILT");
        }
        self.state = State::STARTED;


    }

    pub fn network_context(&self) -> NetworkContext {
        self.network_context.clone()
    }
}

pub struct ApplicationConnections {
    pub protocol_id: ProtocolId,

    /// sender receives messages from network, towards application code
    pub sender: tokio::sync::mpsc::Sender<ReceivedMessage>,

    /// receiver is where application code takes messages from network peers
    //pub receiver: tokio::sync::mpsc::Receiver<ReceivedMessage>,

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

pub struct ApplicationCollector {
    apps: BTreeMap<ProtocolId,ApplicationConnections>,
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

struct PeerManager<TTransport, TSocket>
    where
        TTransport: Transport,
        TSocket: AsyncRead + AsyncWrite,
{
    _ph1 : PhantomData<TTransport>,
    _ph2 : PhantomData<TSocket>,
}

impl<TTransport, TSocket> PeerManager<TTransport, TSocket>
    where
        TTransport: Transport<Output = Connection<TSocket>> + Send + 'static,
        TSocket: aptos_network2::transport::TSocket,
{
    pub fn new(_transport: TTransport) -> Self {
        // TODO network2: rebuild
        Self{
            _ph1: Default::default(),
            _ph2: Default::default(),
        }
    }
}

#[cfg(any(test, feature = "testing", feature = "fuzzing"))]
type MemoryPeerManager =
PeerManager<AptosNetTransport<MemoryTransport>, NoiseStream<aptos_memsocket::MemorySocket>>;
type TcpPeerManager = PeerManager<AptosNetTransport<TcpTransport>, NoiseStream<TcpSocket>>;

enum TransportPeerManager {
    #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
    Memory(MemoryPeerManager),
    Tcp(TcpPeerManager),
}
