// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use std::io::{Chain, Error};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Receiver;
use aptos_config::config::{DiscoveryMethod, HANDSHAKE_VERSION, NetworkConfig, Peer, PeerRole, PeerSet, RoleType};
use aptos_config::network_id::{NetworkContext, NetworkId, PeerNetworkId};
use aptos_crypto::x25519;
use aptos_event_notifications::{DbBackedOnChainConfig,EventSubscriptionService};
use aptos_logger::{error, info, warn};
#[cfg(any(test, feature = "testing", feature = "fuzzing"))]
use aptos_netcore::transport::memory::MemoryTransport;
use aptos_netcore::transport::tcp::{TCPBufferCfg, TcpSocket, TcpTransport};
use aptos_netcore::transport::{ConnectionOrigin, Transport};
use aptos_network2::application::interface::OutboundRpcMatcher;
use aptos_network2::application::metadata::PeerMetadata;
// use aptos_network_discovery::DiscoveryChangeListener;
use aptos_time_service::TimeService;
use aptos_types::chain_id::ChainId;
use aptos_network2::application::storage::PeersAndMetadata;
use aptos_network2::connectivity_manager::{ConnectivityManager, ConnectivityRequest};
use aptos_network2::logging::NetworkSchema;
use aptos_network2::noise::stream::NoiseStream;
use aptos_network2::protocols::wire::handshake::v1::{ProtocolId, ProtocolIdSet};
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;
use aptos_network2::protocols::network::{OutboundPeerConnections, PeerStub, ReceivedMessage};
use aptos_network2::transport::{APTOS_TCP_TRANSPORT, AptosNetTransport, Connection};
use aptos_network_discovery::DiscoveryChangeListener;
use aptos_short_hex_str::AsShortHexStr;
use aptos_types::network_address::{NetworkAddress, Protocol};
use aptos_types::PeerId;
use tokio_retry::strategy::ExponentialBackoff;

mod peer;
// use peer::Peer;

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
    discovery_listeners: Vec<DiscoveryChangeListener<DbBackedOnChainConfig>>,
    // connectivity_manager_builder: Option<ConnectivityManagerBuilder>, // TODO network2: re-enable connectivity manager
    // health_checker_builder: Option<HealthCheckerBuilder>,
    // peer_manager_builder: PeerManagerBuilder,
    peers_and_metadata: Arc<PeersAndMetadata>,
    apps: Arc<ApplicationCollector>,
    peer_senders: Arc<OutboundPeerConnections>,
    handle: Option<Handle>,
    // cm: ConnectivityManager<ExponentialBackoff>,
    // temporarily hold a value from create() until start()
    connectivity_req_rx: Option<tokio::sync::mpsc::Receiver<ConnectivityRequest>>,
}

impl NetworkBuilder {
    /// Create a new NetworkBuilder based on the provided configuration.
    pub fn create(
        chain_id: ChainId,
        role: RoleType,
        config: &NetworkConfig,
        time_service: TimeService,
        reconfig_subscription_service: Option<&mut EventSubscriptionService>,
        peers_and_metadata: Arc<PeersAndMetadata>,
        peer_senders: Arc<OutboundPeerConnections>,
        handle: Option<Handle>,
    ) -> NetworkBuilder {
        let network_context = NetworkContext::new(role, config.network_id, config.peer_id());
        let (connectivity_req_sender, connectivity_req_rx) = tokio::sync::mpsc::channel::<ConnectivityRequest>(10);
        // let seeds = config.merge_seeds();
        // let cm = ConnectivityManager::new(
        //     network_context,
        //     time_service.clone(),
        //     peers_and_metadata.clone(),
        //     seeds,
        //     // connection_reqs_tx,
        //     // connection_notifs_rx,
        //     connectivity_req_rx,
        //     Duration::from_millis(config.connectivity_check_interval_ms),
        //     ExponentialBackoff::from_millis(config.connection_backoff_base).factor(1000),
        //     Duration::from_millis(config.max_connection_delay_ms),
        //     Some(config.max_outbound_connections),
        //     config.mutual_authentication,
        // );
        let mut nb = NetworkBuilder{
            state: State::CREATED,
            executor: None,
            time_service,
            network_context,
            chain_id,
            config: config.clone(),
            discovery_listeners: vec![],
            // connectivity_manager_builder: None,
            peers_and_metadata,
            peer_senders,
            apps: Arc::new(ApplicationCollector::new()), // temporary empty app set
            handle,
            // cm,
            connectivity_req_rx: Some(connectivity_req_rx),
        };
        nb.setup_discovery(reconfig_subscription_service, connectivity_req_sender);
        nb
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

    fn setup_discovery(
        &mut self,
        mut reconfig_subscription_service: Option<&mut EventSubscriptionService>,
        conn_mgr_reqs_tx: tokio::sync::mpsc::Sender<ConnectivityRequest>,
    ) {
        for disco in self.config.discovery_methods().into_iter() {
            let listener = match disco {
                DiscoveryMethod::Onchain => {
                    let reconfig_events = reconfig_subscription_service
                        .as_mut()
                        .expect("An event subscription service is required for on-chain discovery!")
                        .subscribe_to_reconfigurations()
                        .expect("On-chain discovery is unable to subscribe to reconfigurations!");
                    let identity_key = self.config.identity_key();
                    let pubkey = identity_key.public_key();
                    DiscoveryChangeListener::validator_set(
                        self.network_context,
                        conn_mgr_reqs_tx.clone(),
                        pubkey,
                        reconfig_events,
                    )
                }
                DiscoveryMethod::File(file_discovery) => DiscoveryChangeListener::file(
                    self.network_context,
                    conn_mgr_reqs_tx.clone(),
                    file_discovery.path.as_path(),
                    Duration::from_secs(file_discovery.interval_secs),
                    self.time_service.clone(),
                ),
                DiscoveryMethod::Rest(rest_discovery) => DiscoveryChangeListener::rest(
                    self.network_context,
                    conn_mgr_reqs_tx.clone(),
                    rest_discovery.url.clone(),
                    Duration::from_secs(rest_discovery.interval_secs),
                    self.time_service.clone(),
                ),
                DiscoveryMethod::None => {
                    continue;
                }
            };
            self.discovery_listeners.push(listener);
        }
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
        let handle = self.handle.clone().unwrap();
        handle.enter();
        let config = &self.config;
        let seeds = config.merge_seeds();
        let connectivity_req_rx = self.connectivity_req_rx.take().unwrap();
        let cm = ConnectivityManager::new(
            self.network_context,
            self.time_service.clone(),
            self.peers_and_metadata.clone(),
            seeds,
            // connection_reqs_tx,
            // connection_notifs_rx,
            connectivity_req_rx,
            Duration::from_millis(config.connectivity_check_interval_ms),
            ExponentialBackoff::from_millis(config.connection_backoff_base).factor(1000),
            Duration::from_millis(config.max_connection_delay_ms),
            Some(config.max_outbound_connections),
            config.mutual_authentication,
        );
        handle.spawn(cm.start());
        for disco in self.discovery_listeners.drain(..) {
            disco.start(&handle);
        }
        let listen_addr = transport_peer_manager_start(
            self.build_transport(),
            self.config.listen_address.clone(),
            handle,
            self.network_context.network_id(),
            self.apps.clone(),
        );
        info!("network {:?} listening on {:?}", self.network_context.network_id(), listen_addr);
        self.state = State::STARTED;
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
    // config: &NetworkConfig,
    // time_service: TimeService,
    // connectivity_req_rx: tokio::sync::mpsc::Receiver<ConnectivityRequest>,
) -> NetworkAddress {
    let result = match tpm {
        #[cfg(any(test, feature = "testing", feature = "fuzzing"))]
        TransportPeerManager::Memory(mut pm) => { pm.listen( listen_address,  executor ) }
        TransportPeerManager::Tcp(mut pm) => { pm.listen( listen_address,  executor ) }
    };
    match result {
        Ok(listen_address) => { listen_address }
        Err(err) => {
            panic!("could not start network {:?}: {:?}", network_id, err);
        }
    }
}

// TODO: move into network/framework2
pub struct ApplicationConnections {
    pub protocol_id: ProtocolId,

    /// sender receives messages from network, towards application code
    pub sender: tokio::sync::mpsc::Sender<ReceivedMessage>,

    /// label used in metrics counters
    pub label: String,
}

// TODO: move into network/framework2
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

// TODO: move into network/framework2
pub struct ApplicationCollector {
    apps: BTreeMap<ProtocolId,ApplicationConnections>,
}

// TODO: move into network/framework2
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
/// TODO: move into network/framework2
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

// TODO: move into network/framework2
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

    async fn connect_outbound(
        &self,
        peer_id: PeerId,
        addr: NetworkAddress,
    ) {
        // TODO: rebuild connection init time counter
        let outbound = match self.transport.dial(peer_id, addr.clone()) {
            Ok(outbound) => {
                outbound
            }
            Err(err) => {
                warn!("dial err: {:?}", err);
                // TODO: counter
                return;
            }
        };
        let mut connection = match outbound.await {
            Ok(connection) => { // Connection<TSocket>
                connection
            }
            Err(err) => {
                warn!("dial err 2: {:?}", err);
                // TODO: counter
                return
            }
        };
        let dialed_peer_id = connection.metadata.remote_peer_id;
        if dialed_peer_id != peer_id {
            warn!("dial {:?} did not reach peer {:?} but peer {:?}", addr, peer_id, dialed_peer_id);
            connection.socket.close();
            return;
        }
        // TODO: store connection, start stuff
    }

    fn listen(
        mut self,
        // config: &NetworkConfig,
        listen_addr: NetworkAddress,
        // time_service: TimeService,
        executor: Handle,
        // connectivity_req_rx: tokio::sync::mpsc::Receiver<ConnectivityRequest>,
    ) -> Result<NetworkAddress, <TTransport>::Error> {
        // let (sockets, listen_addr_actual) = self.transport.listen_on(listen_addr)?;
        let (sockets, listen_addr_actual) = executor.block_on(self.first_listen(listen_addr))?;
        // let seeds = config.merge_seeds();
        // let cm = ConnectivityManager::new(
        //     self.network_context,
        //     time_service,
        //     self.peers_and_metadata.clone(),
        //     seeds,
        //     // connection_reqs_tx,
        //     // connection_notifs_rx,
        //     connectivity_req_rx,
        //     Duration::from_millis(config.connectivity_check_interval_ms),
        //     ExponentialBackoff::from_millis(config.connection_backoff_base).factor(1000),
        //     Duration::from_millis(config.max_connection_delay_ms),
        //     Some(config.max_outbound_connections),
        //     config.mutual_authentication,
        // );
        // executor.spawn(cm.start());
        executor.spawn(self.listener_thread(sockets, executor.clone()));
        Ok(listen_addr_actual)
    }

    async fn first_listen(&mut self, listen_addr: NetworkAddress) -> Result<(<TTransport>::Listener, NetworkAddress), TTransport::Error> {
        self.transport.listen_on(listen_addr)
    }

    async fn listener_thread(mut self, mut sockets: <TTransport>::Listener, executor: Handle) {
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
                    let (sender, receiver) = tokio::sync::mpsc::channel::<NetworkMessage>(self.config.network_channel_size);
                    let remote_peer_network_id = PeerNetworkId::new(self.network_context.network_id(), connection.metadata.remote_peer_id);
                    self.peers_and_metadata.insert_connection_metadata(remote_peer_network_id, connection.metadata);
                    let open_outbound_rpc = OutboundRpcMatcher::new();
                    // TODO: how do we shut down a peer on disconnect?
                    peer::start_peer(
                        &self.config,
                        connection.socket,
                        receiver,
                        self.apps.clone(),
                        executor.clone(),
                        remote_peer_network_id,
                        open_outbound_rpc.clone());
                    let stub = PeerStub::new(sender, open_outbound_rpc);
                    self.peer_senders.insert(remote_peer_network_id, stub);
                    // TODO: peer connection counter
                    // TODO: peer connection event
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
