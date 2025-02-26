use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Range,
    sync::Arc,
    time::Duration,
};

use ambient_core::{
    asset_cache, name, no_sync,
    player::{get_by_user_id, player},
    project_name,
};
use ambient_ecs::{
    components, dont_store, query, ArchetypeFilter, ComponentDesc, Entity, EntityId, FrameEvent,
    Resource, System, SystemGroup, World, WorldStream, WorldStreamCompEvent, WorldStreamFilter,
};
use ambient_proxy::client::AllocatedEndpoint;
use ambient_rpc::RpcRegistry;
use ambient_std::{
    asset_cache::{AssetCache, SyncAssetKeyExt},
    asset_url::{AbsAssetUrl, ServerBaseUrlKey},
    fps_counter::{FpsCounter, FpsSample},
    friendly_id, log_result,
};
use ambient_sys::time::{Instant, SystemTime};
use anyhow::bail;
use bytes::Bytes;
use flume::Sender;
use futures::StreamExt;
use once_cell::sync::OnceCell;
use parking_lot::{Mutex, RwLock};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::{
    io::AsyncReadExt,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug_span, Instrument};

use crate::{
    client_connection::ClientConnection,
    connection::Connection,
    create_server,
    protocol::{ClientInfo, ServerInfo, ServerProtocol},
    NetworkError, RPC_BISTREAM_ID,
};
use colored::Colorize;

components!("network::server", {
    @[Resource]
    bi_stream_handlers: BiStreamHandlers,
    @[Resource]
    uni_stream_handlers: UniStreamHandlers,
    @[Resource]
    datagram_handlers: DatagramHandlers,

    player_entity_stream: Sender<Vec<u8>>,
    player_stats_stream: Sender<FpsSample>,
    player_connection: ClientConnection,
});

pub type BiStreamHandlers = HashMap<
    u32,
    Arc<dyn Fn(SharedServerState, AssetCache, &String, SendStream, RecvStream) + Sync + Send>,
>;
pub type UniStreamHandlers =
    HashMap<u32, Arc<dyn Fn(SharedServerState, AssetCache, &String, RecvStream) + Sync + Send>>;
pub type DatagramHandlers =
    HashMap<u32, Arc<dyn Fn(SharedServerState, AssetCache, &String, Bytes) + Sync + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct ForkingEvent;

#[derive(Debug, Clone, Copy)]
pub struct ForkedEvent;

#[derive(Debug, Clone, Copy)]
pub struct ShutdownEvent;

pub struct WorldInstance {
    pub world: World,
    pub world_stream: WorldStream,
    pub systems: SystemGroup,
}

#[derive(Clone)]
pub struct RpcArgs {
    pub state: SharedServerState,
    pub user_id: String,
}
impl RpcArgs {
    pub fn get_player(&self, world: &World) -> Option<EntityId> {
        get_by_user_id(world, &self.user_id)
    }
}

pub fn create_player_entity_data(
    user_id: &str,
    entities_tx: Sender<Vec<u8>>,
    stats_tx: Sender<FpsSample>,
) -> Entity {
    Entity::new()
        .with(name(), format!("Player {}", user_id))
        .with(ambient_core::player::player(), ())
        .with(ambient_core::player::user_id(), user_id.to_string())
        .with(player_entity_stream(), entities_tx)
        .with(player_stats_stream(), stats_tx)
        .with_default(dont_store())
}

pub fn register_rpc_bi_stream_handler(
    handlers: &mut BiStreamHandlers,
    rpc_registry: RpcRegistry<RpcArgs>,
) {
    handlers.insert(
        RPC_BISTREAM_ID,
        Arc::new(move |state, _assets, user_id, mut send, recv| {
            let state = state;
            let user_id = user_id.to_string();
            let rpc_registry = rpc_registry.clone();
            tokio::spawn(async move {
                let try_block = || async {
                    let req = recv.read_to_end(100_000_000).await?;
                    let args = RpcArgs {
                        state,
                        user_id: user_id.to_string(),
                    };
                    let resp = rpc_registry.run_req(args, &req).await?;
                    send.write_all(&resp).await?;
                    send.finish().await?;
                    Ok(()) as Result<(), NetworkError>
                };
                log_result!(try_block().await);
            });
        }),
    );
}

impl WorldInstance {
    /// Create server side player entity
    pub fn spawn_player(&mut self, ed: Entity) -> EntityId {
        ed.spawn(&mut self.world)
    }
    pub fn despawn_player(&mut self, user_id: &str) -> Option<Entity> {
        self.world.despawn(get_by_user_id(&self.world, user_id)?)
    }
    pub fn broadcast_diffs(&mut self) {
        let diff = self.world_stream.next_diff(&self.world);
        if diff.is_empty() {
            return;
        }
        let msg = bincode::serialize(&diff).unwrap();

        ambient_profiling::scope!("Send MsgEntities");
        for (_, (entity_stream,)) in query((player_entity_stream(),)).iter(&self.world, None) {
            let msg = msg.clone();
            if let Err(_err) = entity_stream.send(msg) {
                log::warn!("Failed to broadcast diff to player");
            }
        }
    }
    pub fn player_count(&self) -> usize {
        query((player(),)).iter(&self.world, None).count()
    }
    pub fn step(&mut self, time: Duration) {
        self.world
            .set(self.world.resource_entity(), ambient_core::time(), time)
            .unwrap();
        self.systems.run(&mut self.world, &FrameEvent);
        self.world.next_frame();
    }
}

pub const MAIN_INSTANCE_ID: &str = "main";

pub struct Player {
    pub instance: String,
    pub abort_handle: Arc<OnceCell<tokio::task::JoinHandle<()>>>,
    pub connection_id: String,
}

impl Player {
    pub fn new(
        instance: String,
        abort_handle: Arc<OnceCell<tokio::task::JoinHandle<()>>>,
        connection_id: String,
    ) -> Self {
        Self {
            instance,
            abort_handle,
            connection_id,
        }
    }

    pub fn new_local(instance: String) -> Self {
        Self {
            instance,
            abort_handle: Arc::new(OnceCell::new()),
            connection_id: friendly_id(),
        }
    }
}

pub type SharedServerState = Arc<Mutex<ServerState>>;
pub struct ServerState {
    pub instances: HashMap<String, WorldInstance>,
    pub players: HashMap<String, Player>,
    pub create_server_systems: Arc<dyn Fn(&mut World) -> SystemGroup + Sync + Send>,
    pub create_on_forking_systems: Arc<dyn Fn() -> SystemGroup<ForkingEvent> + Sync + Send>,
    pub create_shutdown_systems: Arc<dyn Fn() -> SystemGroup<ShutdownEvent> + Sync + Send>,
}
impl ServerState {
    pub fn new_local() -> Self {
        let world_stream_filter =
            WorldStreamFilter::new(ArchetypeFilter::new(), Arc::new(|_, _| false));
        Self {
            instances: [(
                MAIN_INSTANCE_ID.to_string(),
                WorldInstance {
                    world: World::new("main_server"),
                    world_stream: WorldStream::new(world_stream_filter),
                    systems: SystemGroup::new("", vec![]),
                },
            )]
            .into(),
            players: Default::default(),
            create_server_systems: Arc::new(|_| SystemGroup::new("", vec![])),
            create_on_forking_systems: Arc::new(|| SystemGroup::new("", vec![])),
            create_shutdown_systems: Arc::new(|| SystemGroup::new("", vec![])),
        }
    }
    pub fn new(
        instances: HashMap<String, WorldInstance>,
        create_server_systems: Arc<dyn Fn(&mut World) -> SystemGroup + Sync + Send>,
        create_on_forking_systems: Arc<dyn Fn() -> SystemGroup<ForkingEvent> + Sync + Send>,
        create_shutdown_systems: Arc<dyn Fn() -> SystemGroup<ShutdownEvent> + Sync + Send>,
    ) -> Self {
        Self {
            instances,
            players: Default::default(),
            create_server_systems,
            create_on_forking_systems,
            create_shutdown_systems,
        }
    }

    pub fn step(&mut self) {
        let time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        for instance in self.instances.values_mut() {
            instance.step(time);
        }
    }
    pub fn broadcast_diffs(&mut self) {
        for instance in self.instances.values_mut() {
            instance.broadcast_diffs();
        }
    }
    pub fn player_count(&self) -> usize {
        self.instances.values().map(|i| i.player_count()).sum()
    }
    pub fn get_player_world_instance_mut(&mut self, user_id: &str) -> Option<&mut WorldInstance> {
        self.players
            .get(user_id)
            .and_then(|player| self.instances.get_mut(&player.instance))
    }
    pub fn get_player_world_instance(&self, user_id: &str) -> Option<&WorldInstance> {
        self.players
            .get(user_id)
            .and_then(|player| self.instances.get(&player.instance))
    }
    pub fn get_player_world_mut(&mut self, user_id: &str) -> Option<&mut World> {
        self.get_player_world_instance_mut(user_id)
            .map(|i| &mut i.world)
    }
    pub fn get_player_world(&self, user_id: &str) -> Option<&World> {
        self.get_player_world_instance(user_id).map(|i| &i.world)
    }
    pub fn remove_instance(&mut self, instance_id: &str) {
        log::debug!("Removing server instance id={}", instance_id);
        let mut sys = (self.create_shutdown_systems)();
        let old_instance = self.instances.get_mut(instance_id).unwrap();
        sys.run(&mut old_instance.world, &ShutdownEvent);
        self.instances.remove(instance_id);
    }
}

#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub endpoint: String,
    pub project_path: AbsAssetUrl,
    pub pre_cache_assets: bool,
    pub project_id: String,
}

pub struct GameServer {
    endpoint: Endpoint,
    pub port: u16,
    /// Shuts down the server if there are no players
    pub use_inactivity_shutdown: bool,
    proxy_settings: Option<ProxySettings>,
}
impl GameServer {
    pub async fn new_with_port(
        port: u16,
        use_inactivity_shutdown: bool,
        proxy_settings: Option<ProxySettings>,
    ) -> anyhow::Result<Self> {
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);

        let endpoint = create_server(server_addr)?;

        log::debug!("GameServer listening on port {}", port);
        Ok(Self {
            endpoint,
            port,
            use_inactivity_shutdown,
            proxy_settings,
        })
    }
    pub async fn new_with_port_in_range(
        port_range: Range<u16>,
        use_inactivity_shutdown: bool,
        proxy_settings: Option<ProxySettings>,
    ) -> anyhow::Result<Self> {
        for port in port_range {
            match Self::new_with_port(port, use_inactivity_shutdown, proxy_settings.clone()).await {
                Ok(server) => {
                    return Ok(server);
                }
                Err(_err) => {
                    log::warn!("Failed to create server on port {}", port);
                }
            }
        }
        bail!("Failed to create server")
    }
    #[tracing::instrument(skip_all)]
    pub async fn run(
        self,
        mut world: World,
        create_server_systems: Arc<dyn Fn(&mut World) -> SystemGroup + Sync + Send>,
        create_on_forking_systems: Arc<dyn Fn() -> SystemGroup<ForkingEvent> + Sync + Send>,
        create_shutdown_systems: Arc<dyn Fn() -> SystemGroup<ShutdownEvent> + Sync + Send>,
        is_sync_component: Arc<dyn Fn(ComponentDesc, WorldStreamCompEvent) -> bool + Sync + Send>,
    ) -> SharedServerState {
        let Self {
            endpoint,
            proxy_settings,
            ..
        } = self;
        let assets = world.resource(asset_cache()).clone();
        let world_stream_filter =
            WorldStreamFilter::new(ArchetypeFilter::new().excl(no_sync()), is_sync_component);
        let state = Arc::new(Mutex::new(ServerState::new(
            [(
                MAIN_INSTANCE_ID.to_string(),
                WorldInstance {
                    systems: create_server_systems(&mut world),
                    world,
                    world_stream: WorldStream::new(world_stream_filter.clone()),
                },
            )]
            .into_iter()
            .collect(),
            create_server_systems,
            create_on_forking_systems,
            create_shutdown_systems,
        )));

        let mut fps_counter = FpsCounter::new();
        let mut sim_interval = interval(Duration::from_secs_f32(1. / 60.));
        sim_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut inactivity_interval = interval(Duration::from_secs_f32(5.));
        let mut last_active = ambient_sys::time::Instant::now();

        if let Some(proxy_settings) = proxy_settings {
            let endpoint = endpoint.clone();
            let state = state.clone();
            let world_stream_filter = world_stream_filter.clone();
            let assets = assets.clone();
            tokio::spawn(async move {
                start_proxy_connection(
                    endpoint.clone(),
                    proxy_settings,
                    state.clone(),
                    world_stream_filter.clone(),
                    assets.clone(),
                )
                .await;
            });
        }

        loop {
            tracing::debug_span!("Listening for incoming connections");
            tokio::select! {
                Some(conn) = endpoint.accept() => {
                    log::debug!("Received connection");

                    let conn = match conn.await {
                        Ok(v) => v,
                        Err(e) => {
                            log::error!("Failed to accept incoming connection. {e}");
                            continue;
                        }
                    };


                    log::debug!("Accepted connection");
                    run_connection(conn.into(), state.clone(), world_stream_filter.clone(), assets.clone(), ServerBaseUrlKey.get(&assets));
                }
                _ = sim_interval.tick() => {
                    fps_counter.frame_start();
                    let mut state = state.lock();
                    tokio::task::block_in_place(|| {
                        ambient_profiling::finish_frame!();
                        ambient_profiling::scope!("sim_tick");
                        state.step();
                        state.broadcast_diffs();
                        if let Some(sample) = fps_counter.frame_end() {
                            for instance in state.instances.values() {
                                for (_, (stream,)) in query((player_stats_stream(),)).iter(&instance.world, None) {
                                    stream.send(sample.clone()).ok();
                                }
                            }
                        }
                    });
                }
                _ = inactivity_interval.tick(), if self.use_inactivity_shutdown => {
                    if state.lock().player_count() == 0 {
                        if Instant::now().duration_since(last_active).as_secs_f32() > 2. * 60. {
                            log::info!("[{}] Shutting down due to inactivity", self.port);
                            break;
                        }
                    } else {
                        last_active = Instant::now();
                    }
                }
                else => {
                    log::info!("No more connections. Shutting down.");
                    break
                }
            }
        }
        log::debug!("[{}] GameServer shutting down", self.port);
        {
            let mut state = state.lock();
            let create_shutdown_systems = state.create_shutdown_systems.clone();
            for instance in state.instances.values_mut() {
                let mut sys = (create_shutdown_systems)();
                sys.run(&mut instance.world, &ShutdownEvent);
            }
        }
        log::debug!("[{}] GameServer finished shutting down", self.port);
        state
    }
}

async fn start_proxy_connection(
    endpoint: Endpoint,
    settings: ProxySettings,
    state: Arc<Mutex<ServerState>>,
    world_stream_filter: WorldStreamFilter,
    assets: AssetCache,
) {
    // start with content base url being the same as for direct connections
    let content_base_url = Arc::new(RwLock::new(ServerBaseUrlKey.get(&assets)));

    let on_endpoint_allocated = {
        let content_base_url = content_base_url.clone();
        Arc::new(
            move |AllocatedEndpoint {
                      id,
                      allocated_endpoint,
                      external_endpoint,
                      assets_root,
                      ..
                  }: AllocatedEndpoint| {
                log::debug!("Allocated proxy endpoint. Allocation id: {}", id);
                log::info!("Proxy sees this server as {}", external_endpoint);
                log::info!(
                    "Proxy allocated an endpoint, use `{}` to join",
                    format!("ambient join {}", allocated_endpoint).bright_green()
                );

                // set the content base url to point to proxy provided value
                match AbsAssetUrl::parse(&assets_root) {
                    Ok(url) => {
                        log::debug!("Got content base root from proxy: {}", url);
                        *content_base_url.write() = url;
                    }
                    Err(err) => {
                        log::warn!("Failed to parse assets root url ({}): {}", assets_root, err)
                    }
                }
            },
        )
    };

    let on_player_connected = {
        let assets = assets.clone();
        let content_base_url = content_base_url.clone();
        Arc::new(
            move |_player_id, conn: ambient_proxy::client::ProxiedConnection| {
                log::debug!("Accepted connection via proxy");
                run_connection(
                    conn.into(),
                    state.clone(),
                    world_stream_filter.clone(),
                    assets.clone(),
                    content_base_url.read().clone(),
                );
            },
        )
    };

    static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

    let builder = ambient_proxy::client::builder()
        .endpoint(endpoint.clone())
        .proxy_server(settings.endpoint.clone())
        .project_id(settings.project_id.clone())
        .user_agent(APP_USER_AGENT.to_string());

    let assets_path = settings.project_path.push("build").expect("Pushing to path cannot fail");
    let builder = if let Ok(Some(assets_file_path)) = assets_path.to_file_path() {
        builder.assets_path(assets_file_path)
    } else {
        builder.assets_root_override(content_base_url.read().to_string())
    };

    log::info!("Connecting to proxy server");
    let proxy = match builder.build().await {
        Ok(proxy_client) => proxy_client,
        Err(err) => {
            log::warn!("Failed to connect to proxy: {}", err);
            return;
        }
    };

    // start and allocate endpoint
    let mut controller = proxy.start(on_endpoint_allocated, on_player_connected);
    log::info!("Allocating proxy endpoint");
    if let Err(err) = controller.allocate_endpoint().await {
        log::warn!("Failed to allocate proxy endpoint: {}", err);
    }

    // pre-cache "assets" subdirectory
    if settings.pre_cache_assets {
        for subdir in ["assets", "client"] {
            if let Err(err) = controller.pre_cache_assets(subdir) {
                log::warn!("Failed to pre-cache assets: {}", err);
            }
        }
    }
}

/// Setup the protocol and enter the update loop for a new connected client
#[tracing::instrument(skip_all)]
fn run_connection(
    connection: ClientConnection,
    state: SharedServerState,
    world_stream_filter: WorldStreamFilter,
    assets: AssetCache,
    content_base_url: AbsAssetUrl,
) {
    let connection_id = friendly_id();
    let handle = Arc::new(OnceCell::new());
    handle
        .set({
            let handle = handle.clone();
            tokio::spawn(async move {
                let (diffs_tx, diffs_rx) = flume::unbounded();
                let (stats_tx, stats_rx) = flume::unbounded();

                let new_player_connection = connection.clone();

                let on_init = |client: ClientInfo| {
                    let user_id = &client.user_id;
                    log::debug!("[{}] Locking world", user_id);
                    let mut state = state.lock();
                    // If there's an old player
                    let reconnecting = if let Some(player) = state.players.get_mut(user_id) {
                        if let Some(handle) = player.abort_handle.get() {
                            handle.abort();
                        }
                        player.abort_handle = handle.clone();
                        player.connection_id = connection_id.clone();
                        log::debug!("[{}] Player reconnecting", user_id);
                        true
                    } else {
                        state.players.insert(
                            user_id.clone(),
                            Player {
                                instance: MAIN_INSTANCE_ID.to_string(),
                                abort_handle: handle.clone(),
                                connection_id: connection_id.clone(),
                            },
                        );
                        false
                    };

                    let instance = state.instances.get_mut(MAIN_INSTANCE_ID).unwrap();

                    // Bring world stream up to the current time
                    log::debug!("[{}] Broadcasting diffs", user_id);
                    instance.broadcast_diffs();
                    log::debug!("[{}] Creating init diff", user_id);

                    let diff = world_stream_filter.initial_diff(&instance.world);
                    let diff = bincode::serialize(&diff).unwrap();

                    log_result!(diffs_tx.send(diff));
                    log::debug!("[{}] Init diff sent", user_id);

                    if !reconnecting {
                        instance.spawn_player(
                            create_player_entity_data(user_id, diffs_tx.clone(), stats_tx.clone())
                                .with(player_connection(), new_player_connection.clone()),
                        );
                        log::info!("[{}] Player spawned", user_id);
                    } else {
                        let entity = get_by_user_id(&instance.world, user_id).unwrap();
                        instance
                            .world
                            .set(entity, player_entity_stream(), diffs_tx.clone())
                            .unwrap();
                        instance
                            .world
                            .set(entity, player_stats_stream(), stats_tx.clone())
                            .unwrap();
                        instance
                            .world
                            .set(entity, player_connection(), new_player_connection.clone())
                            .unwrap();
                        log::info!("[{}] Player reconnected", user_id);
                    }
                };

                let on_disconnect = |user_id: &Option<String>| {
                    if let Some(user_id) = user_id {
                        log::debug!("[{}] Disconnecting", user_id);
                        let mut state = state.lock();
                        if state
                            .players
                            .get(user_id)
                            .map(|p| p.connection_id != connection_id)
                            .unwrap_or(false)
                        {
                            log::info!("[{}] Disconnected (reconnection)", user_id);
                            return;
                        }
                        if let Some(player) = state.players.remove(user_id) {
                            state
                                .instances
                                .get_mut(&player.instance)
                                .unwrap()
                                .despawn_player(user_id);
                        }

                        log::info!("[{}] Disconnected", user_id);
                    }
                };

                let on_bi_stream = |user_id: &String, handler_id, tx, rx| {
                    let _span = debug_span!("on_bi_stream").entered();
                    let handler = {
                        let state = state.lock();
                        let world = match state.get_player_world(user_id) {
                            Some(world) => world,
                            None => {
                                log::error!("Player missing for rpc."); // Probably disconnected
                                return;
                            }
                        };

                        world
                            .resource(bi_stream_handlers())
                            .get(&handler_id)
                            .cloned()
                    };
                    if let Some(handler) = handler {
                        handler(state.clone(), assets.clone(), user_id, tx, rx);
                    } else {
                        log::error!("Unrecognized stream handler id: {}", handler_id);
                    }
                };

                let on_uni_stream = |user_id: &String, handler_id, rx| {
                    let _span = debug_span!("on_uni_stream").entered();
                    let handler = {
                        let state = state.lock();
                        let world = match state.get_player_world(user_id) {
                            Some(world) => world,
                            None => {
                                log::error!("Player missing for rpc."); // Probably disconnected
                                return;
                            }
                        };

                        world
                            .resource(uni_stream_handlers())
                            .get(&handler_id)
                            .cloned()
                    };
                    if let Some(handler) = handler {
                        handler(state.clone(), assets.clone(), user_id, rx);
                    } else {
                        log::error!("Unrecognized stream handler id: {}", handler_id);
                    }
                };

                let on_datagram = |user_id: &String, handler_id: u32, bytes: Bytes| {
                    let state = state.clone();
                    let handler = {
                        let state = state.lock();
                        let world = match state.get_player_world(user_id) {
                            Some(world) => world,
                            None => {
                                log::warn!("Player missing for datagram."); // Probably disconnected
                                return;
                            }
                        };
                        world
                            .resource(datagram_handlers())
                            .get(&handler_id)
                            .cloned()
                    };
                    match handler {
                        Some(handler) => {
                            handler(state, assets.clone(), user_id, bytes);
                        }
                        None => {
                            log::error!("No such datagram handler: {:?}", handler_id);
                        }
                    }
                };

                let client = ClientInstance {
                    diffs_rx,
                    stats_rx,
                    on_init: &on_init,
                    on_bi_stream: &on_bi_stream,
                    on_uni_stream: &on_uni_stream,
                    on_datagram: &on_datagram,
                    on_disconnect: &on_disconnect,
                    user_id: None,
                };

                let server_info = {
                    let state = state.lock();
                    let instance = state.instances.get(MAIN_INSTANCE_ID).unwrap();
                    let world = &instance.world;
                    ServerInfo {
                        project_name: world.resource(project_name()).clone(),
                        content_base_url,
                        ..Default::default()
                    }
                };

                match client.run(connection, server_info).await {
                    Ok(()) => {}
                    Err(err) if err.is_closed() => {
                        log::info!("Connection closed by client");
                    }
                    Err(err) if err.is_end_of_stream() => {
                        log::warn!("Stream was closed prematurely");
                    }
                    Err(NetworkError::IOError(err))
                        if err.kind() == std::io::ErrorKind::NotConnected =>
                    {
                        log::warn!("Not connected: {err:?}");
                    }
                    Err(err) => {
                        log::error!("Server error: {err:?}");
                    }
                };
            })
        })
        .expect("Player handle set twice");
}

/// Manages the server side client communication
struct ClientInstance<'a> {
    diffs_rx: flume::Receiver<Vec<u8>>,
    stats_rx: flume::Receiver<FpsSample>,

    on_init: &'a (dyn Fn(ClientInfo) + Send + Sync),
    on_datagram: &'a (dyn Fn(&String, u32, Bytes) + Send + Sync),
    on_bi_stream: &'a (dyn Fn(&String, u32, SendStream, RecvStream) + Send + Sync),
    on_uni_stream: &'a (dyn Fn(&String, u32, RecvStream) + Send + Sync),
    on_disconnect: &'a (dyn Fn(&Option<String>) + Send + Sync),
    user_id: Option<String>,
}

impl<'a> Drop for ClientInstance<'a> {
    fn drop(&mut self) {
        log::debug!("Closed server-side connection for {:?}", self.user_id);
        tokio::task::block_in_place(|| {
            (self.on_disconnect)(&self.user_id);
        })
    }
}

impl<'a> ClientInstance<'a> {
    #[tracing::instrument(skip_all)]
    pub async fn run(
        mut self,
        conn: ClientConnection,
        server_info: ServerInfo,
    ) -> Result<(), NetworkError> {
        log::debug!("Connecting to client");
        let mut proto = ServerProtocol::new(conn, server_info).await?;

        log::debug!("Client loop starting");
        let mut entities_rx = self.diffs_rx.stream();
        let mut stats_rx = self.stats_rx.stream();

        tokio::task::block_in_place(|| {
            (self.on_init)(proto.client_info().clone());
        });
        let user_id = proto.client_info().user_id.clone();
        self.user_id = Some(user_id.clone());

        loop {
            tokio::select! {
                Some(msg) = entities_rx.next() => {
                    let span = tracing::debug_span!("world diff");
                    proto.diff_stream.send_bytes(msg).instrument(span).await?;
                }
                Some(msg) = stats_rx.next() => {
                    let span = tracing::debug_span!("stats");
                    proto.stat_stream.send(&msg).instrument(span).await?;
                }

                Ok(mut datagram) = proto.conn.read_datagram() => {
                    let _span = tracing::debug_span!("datagram").entered();
                    let data = datagram.split_off(4);
                    let handler_id = u32::from_be_bytes(datagram[0..4].try_into().unwrap());
                    tokio::task::block_in_place(|| (self.on_datagram)(&user_id, handler_id, data))
                }
                Ok((tx, mut rx)) = proto.conn.accept_bi() => {
                    let span = tracing::debug_span!("bistream");
                    let stream_id = rx.read_u32().instrument(span).await;
                    if let Ok(stream_id) = stream_id {
                        tokio::task::block_in_place(|| { (self.on_bi_stream)(&user_id, stream_id, tx, rx); })
                    }
                }
                Ok(mut rx) = proto.conn.accept_uni() => {
                    let span = tracing::debug_span!("unistream");
                    let stream_id = rx.read_u32().instrument(span).await;
                    if let Ok(stream_id) = stream_id {
                        tokio::task::block_in_place(|| { (self.on_uni_stream)(&user_id, stream_id,  rx); })
                    }
                }
            }
        }
    }
}
