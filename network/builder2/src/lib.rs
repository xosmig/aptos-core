// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use std::io::{Chain, Error};
use std::marker::PhantomData;
use std::sync::Arc;
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Receiver;
use aptos_config::config::{HANDSHAKE_VERSION, NetworkConfig, PeerRole, RoleType};
use aptos_config::network_id::{NetworkContext, NetworkId, PeerNetworkId};
use aptos_crypto::x25519;
use aptos_event_notifications::{DbBackedOnChainConfig,EventSubscriptionService};
use aptos_logger::{error, info, warn};
#[cfg(any(test, feature = "testing", feature = "fuzzing"))]
use aptos_netcore::transport::memory::MemoryTransport;
use aptos_netcore::transport::tcp::{TCPBufferCfg, TcpSocket, TcpTransport};
use aptos_netcore::transport::{ConnectionOrigin, Transport};
use aptos_network2::application::metadata::PeerMetadata;
// use aptos_network_discovery::DiscoveryChangeListener;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use aptos_network2::application::storage::PeersAndMetadata;
use aptos_network2::logging::NetworkSchema;
use aptos_network2::noise::stream::NoiseStream;
use aptos_network2::protocols::wire::handshake::v1::{ProtocolId, ProtocolIdSet};
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;
use aptos_network2::protocols::network::{OutboundPeerConnections, PeerStub, ReceivedMessage};
use aptos_network2::transport::{APTOS_TCP_TRANSPORT, AptosNetTransport, Connection};
use aptos_short_hex_str::AsShortHexStr;
use aptos_types::network_address::{NetworkAddress, Protocol};

mod peer;

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
    peer_senders: Arc<OutboundPeerConnections>,
    handle: Option<Handle>,
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
        peer_senders: Arc<OutboundPeerConnections>,
    ) -> NetworkBuilder {
        let network_context = NetworkContext::new(role, config.network_id, config.peer_id());
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
            peer_senders,
            apps: Arc::new(ApplicationCollector::new()), // temporary empty app set
            handle: None,
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

    // TODO network2: use this or move it where we need it
    pub fn deliver_message(&self, protocol_id: &ProtocolId, msg: ReceivedMessage) {
        match self.apps.apps.get(protocol_id) {
            None => {
                // TODO network2: peer sent to a ProtocolId we don't process. log error. disconnect. inc a counter
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
        self.handle = Some(handle);
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
                let pm = PeerManager::new(ant, self.peers_and_metadata.clone(), self.config.clone(), self.network_context, self.apps.clone(), self.peer_senders.clone());
                TransportPeerManager::Tcp(pm)
            }
            #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
            Protocol::Memory(_) => {
                let ant = AptosNetTransport::new(
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
                );
                let pm = PeerManager::new(ant, self.peers_and_metadata.clone(), self.config.clone(), self.network_context, self.apps.clone(), self.peer_senders.clone());
                TransportPeerManager::Memory(pm)
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
        let listen_addr = transport_peer_manager_start(
            self.build_transport(),
            self.config.listen_address.clone(),
            self.handle.clone().unwrap(),
            self.network_context.network_id(),
            self.apps.clone(),
        );
        info!("network {:?} listening on {:?}", self.network_context.network_id(), listen_addr);
        // self.state = State::STARTED;
        // self.config.listen_address.clone()
        // // authentication_mode,
        // self.config.mutual_authentication
        // self.config.max_frame_size
        // self.config.max_message_size,
        // self.config.enable_proxy_protocol,
        // self.config.network_channel_size,
        // self.config.max_concurrent_network_reqs,
        // self.config.max_inbound_connections,
    }

    pub fn network_context(&self) -> NetworkContext {
        self.network_context.clone()
    }
}

// single function to wrap variants of templated enum
fn transport_peer_manager_start(
    tpm: TransportPeerManager,
    listen_address: NetworkAddress,
    executor: Handle,
    network_id: NetworkId,
    apps: Arc<ApplicationCollector>,
) -> NetworkAddress {
    let result = match tpm {
        #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
        TransportPeerManager::Memory(mut pm) => { pm.listen(listen_address, executor ) }
        TransportPeerManager::Tcp(mut pm) => { pm.listen(listen_address, executor ) }
    };
    match result {
        Ok(listen_address) => { listen_address }
        Err(err) => {
            panic!("could not start network {:?}: {:?}", network_id, err);
        }
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

/// PeerManager might be more correctly "peer listener" in the new framework?
struct PeerManager<TTransport, TSocket>
    where
        TTransport: Transport,
        TSocket: AsyncRead + AsyncWrite,
{
    transport: TTransport,
    peers_and_metadata: Arc<PeersAndMetadata>,
    peer_cache: Vec<(PeerNetworkId,PeerMetadata)>,
    peer_cache_generation: u32,
    config: NetworkConfig,
    network_context: NetworkContext,
    apps: Arc<ApplicationCollector>,
    peer_senders: Arc<OutboundPeerConnections>,
    _ph2 : PhantomData<TSocket>,
}

impl<TTransport, TSocket> PeerManager<TTransport, TSocket>
    where
        TTransport: Transport<Output = Connection<TSocket>> + Send + 'static,
        TSocket: aptos_network2::transport::TSocket,
{
    pub fn new(
        transport: TTransport,
        peers_and_metadata: Arc<PeersAndMetadata>,
        config: NetworkConfig,
        network_context: NetworkContext,
        apps: Arc<ApplicationCollector>,
        peer_senders: Arc<OutboundPeerConnections>,
    ) -> Self {
        // TODO network2: rebuild
        Self{
            transport,
            peers_and_metadata,
            peer_cache: vec![],
            peer_cache_generation: 0,
            config,
            network_context,
            apps,
            peer_senders,
            _ph2: Default::default(),
        }
    }

    fn maybe_update_peer_cache(&mut self) {
        // if no update is needed, this should be very fast
        // otherwise make copy of peers for use by this thread/task
        if let Some((update, update_generation)) = self.peers_and_metadata.get_all_peers_and_metadata_generational(self.peer_cache_generation, true, &[]) {
            self.peer_cache = update;
            self.peer_cache_generation = update_generation;
        }
    }

    fn listen(
        mut self,
        listen_addr: NetworkAddress,
        executor: Handle,
    ) -> Result<NetworkAddress, <TTransport>::Error> {
        let (sockets, listen_addr_actual) = self.transport.listen_on(listen_addr)?;
        executor.spawn(self.listener_thread(sockets));
        Ok(listen_addr_actual)
    }

    async fn listener_thread(mut self, mut sockets: <TTransport>::Listener) {
        // TODO: leave some connection that can close and shutdown this listener?
        loop {
            let (conn_fut, remote_addr) = match sockets.next().await {
                Some(result) => match result {
                    Ok(conn) => { conn }
                    Err(err) => {
                        error!("listener_thread {:?} got err {:?}, exiting", self.config.network_id, err);
                        return;
                    }
                }
                None => {
                    info!("listener_thread {:?} got None, assuming source closed, exiting", self.config.network_id, );
                    return;
                }
            };
            match conn_fut.await {
                Ok(mut connection) => {
                    let ok = self.check_new_inbound_connection(&connection);
                    info!("got connection {:?}, ok={:?}", remote_addr, ok);
                    if !ok {
                        // conted and logged inside check function above, just close here and be done.
                        connection.socket.close();
                        continue;
                    }
                    // let (sender, receiver) = tokio::sync::mpsc::channel::<ReceivedMessage>(self.config.network_channel_size);
                    let (sender, receiver) = tokio::sync::mpsc::channel::<NetworkMessage>(self.config.network_channel_size);
                    // TODO: make a peer! do protocol stuff!
                    let remote_peer_network_id = PeerNetworkId::new(self.network_context.network_id(), connection.metadata.remote_peer_id);
                    self.peers_and_metadata.insert_connection_metadata(remote_peer_network_id, connection.metadata);
                    // TODO: keep a map<peer network id,PeerStub> to route outbound messages app->socket
                    // TODO: connect to existing map<ProtocolId,Sender<ReceivedMessage>> for inbound messages socket->app
                    let apps = self.apps.clone();
                    let peers_and_metadata = self.peers_and_metadata.clone();
                    let stub = PeerStub::new(sender);
                    self.peer_senders.insert(remote_peer_network_id, stub);
                }
                Err(err) => {
                    error!("listener_thread {:?} connection post-processing failed (continuing): {:?}", self.config.network_id, err);
                }
            }
        }
    }

    // is the new inbound connection okay? => true
    // no, we should disconnect => false
    fn check_new_inbound_connection(&mut self, conn: &Connection<TSocket>) -> bool {
        // Everything below here is meant for unknown peers only. The role comes from
        // the Noise handshake and if it's not `Unknown` then it is trusted.
        // TODO: do more checking for 'trusted' peers
        if conn.metadata.role != PeerRole::Unknown {
            return true;
        }

        // Count unknown inbound connections
        self.maybe_update_peer_cache();
        let mut unknown_inbound_conns = 0;
        let mut already_connected = false;
        let remote_peer_id = conn.metadata.remote_peer_id;

        if remote_peer_id == self.network_context.peer_id() {
            debug_assert!(false, "Self dials shouldn't happen");
            warn!(
                NetworkSchema::new(&self.network_context)
                    .connection_metadata_with_address(&conn.metadata),
                "Received self-dial, disconnecting it"
            );
            return false;
        }

        for wat in self.peer_cache.iter() {
            if wat.0.peer_id() == remote_peer_id {
                already_connected = true;
            }
            let remote_metadata = wat.1.get_connection_metadata();
            if remote_metadata.origin == ConnectionOrigin::Inbound && remote_metadata.role == PeerRole::Unknown {
                unknown_inbound_conns += 1;
            }
        }

        // Reject excessive inbound connections made by unknown peers
        // We control outbound connections with Connectivity manager before we even send them
        // and we must allow connections that already exist to pass through tie breaking.
        if !already_connected
            && unknown_inbound_conns + 1 > self.config.max_inbound_connections
        {
            info!(
                NetworkSchema::new(&self.network_context)
                .connection_metadata_with_address(&conn.metadata),
                "{} Connection rejected due to connection limit: {}",
                self.network_context,
                conn.metadata
            );
            // TODO network2: rebuild counters
            // counters::connections_rejected(&self.network_context, conn.metadata.origin)
            //     .inc();
            return false;
        }

        if already_connected {
            // TODO network2: old code at network/framework/src/peer_manager/mod.rs PeerManager::add_peer() line 615 had provision for sometimes keeping the new connection, but this simplifies and always _drops_ the new connection
            info!(
                NetworkSchema::new(&self.network_context)
                .connection_metadata_with_address(&conn.metadata),
                "{} Closing incoming connection with Peer {} which is already connected",
                self.network_context,
                remote_peer_id.short_str()
            );
            return false;
        }

        return true;
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
