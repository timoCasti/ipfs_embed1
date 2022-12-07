//! Ipfs embed is a small, fast and reliable ipfs implementation designed for
//! embedding in to complex p2p applications.
//!
//! ```no_run
//! # #[async_std::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # use ipfs_embed::{Config, DefaultParams, Ipfs};
//! let mut ipfs = Ipfs::<DefaultParams>::new(Config::default()).await?;
//! ipfs.listen_on("/ip4/0.0.0.0/tcp/0".parse()?);
//! # Ok(()) }
//! ```

mod db;
mod executor;
mod net;
#[cfg(feature = "telemetry")]
mod telemetry;
#[cfg(test)]
mod test_util;
mod variable;

/// convenience re-export of configuration types from libp2p
pub mod config {
    pub use libp2p::{
        dns::{ResolverConfig, ResolverOpts},
        gossipsub::GossipsubConfig,
        identify::Config as IdentifyConfig,
        kad::record::store::MemoryStoreConfig as KadConfig,
        mdns::MdnsConfig,
        ping::Config as PingConfig,
    };
    pub use libp2p_bitswap::BitswapConfig;
    pub use libp2p_broadcast::BroadcastConfig;
}

#[cfg(feature = "telemetry")]
pub use crate::telemetry::telemetry;
pub use crate::{
    db::{Batch, StorageConfig, StorageService, TempPin},
    executor::Executor,
    net::{
        AddressSource, ConnectionFailure, Direction, DnsConfig, Event, GossipEvent, ListenerEvent,
        NetworkConfig, PeerInfo, Rtt, SwarmEvents, SyncEvent, SyncQuery,
    },
};

pub use libipld::{store::DefaultParams, Block, Cid};
pub use libp2p::{
    core::{transport::ListenerId, ConnectedPoint, Multiaddr, PeerId},
    identity,
    kad::{kbucket::Key as BucketKey, record::Key, PeerRecord, Quorum, Record},
    multiaddr,
    swarm::{AddressRecord, AddressScore},
};

use crate::net::NetworkService;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{stream::Stream, Future};
use libipld::{
    codec::References,
    error::BlockNotFound,
    store::{ StoreParams, Store},
    Ipld, Result,
};
use libp2p::identity::ed25519::{Keypair, PublicKey};
use libp2p_bitswap::BitswapStore;
use parking_lot::Mutex;
use prometheus::Registry;
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

/// Ipfs configuration.
#[derive(Debug)]
pub struct Config {
    /// Storage configuration.
    pub storage: StorageConfig,
    /// Network configuration.
    pub network: NetworkConfig,
}

impl Config {
    /// Creates a default configuration from a `path` and a `cache_size`. If the
    /// `path` is `None`, ipfs will use an in-memory block store.
    pub fn new(path: &Path, keypair: Keypair) -> Self {
        let sweep_interval = std::time::Duration::from_millis(10000);
        let storage = StorageConfig::new(Some(path.join("blocks")), None, 0, sweep_interval);
        let network = NetworkConfig::new(keypair);
        Self { storage, network }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new(Path::new("."), Keypair::generate())
    }
}

/// Ipfs node.
#[derive(Clone)]
pub struct Ipfs<P: StoreParams> {
    storage: StorageService<P>,
    network: NetworkService,
}

impl<P: StoreParams> std::fmt::Debug for Ipfs<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Ipfs").finish()
    }
}

struct BitswapStorage<P: StoreParams>(StorageService<P>);

impl<P: StoreParams> BitswapStore for BitswapStorage<P>
where
    Ipld: References<P::Codecs>,
{
    type Params = P;

    fn contains(&mut self, cid: &Cid) -> Result<bool> {
        self.0.contains(cid)
    }

    fn get(&mut self, cid: &Cid) -> Result<Option<Vec<u8>>> {
        self.0.get(cid)
    }

    fn insert(&mut self, block: &Block<P>) -> Result<()> {
        self.0.insert(block.clone())
    }

    fn missing_blocks(&mut self, cid: &Cid) -> Result<Vec<Cid>> {
        self.0.missing_blocks(cid)
    }
}

impl<P: StoreParams> Ipfs<P>
where
    Ipld: References<P::Codecs>,
{
    /// Creates a new `Ipfs` from a `Config`.
    ///
    /// This starts three background tasks. The swarm, garbage collector and the
    /// dht cleanup tasks run in the background.
    pub async fn new(config: Config) -> Result<Self> {
        let executor = Executor::new();
        Self::new0(config, executor).await
    }
    async fn new0(config: Config, executor: Executor) -> Result<Self> {
        let storage = StorageService::open(config.storage, executor.clone())?;
        let bitswap = BitswapStorage(storage.clone());
        let network = NetworkService::new(config.network, bitswap, executor).await?;
        Ok(Self { storage, network })
    }

    /// Returns the local `PublicKey`.
    pub fn local_public_key(&self) -> PublicKey {
        self.network.local_public_key()
    }

    /// Returns the local `PeerId`.
    pub fn local_peer_id(&self) -> PeerId {
        self.network.local_peer_id()
    }

    /// Returns the local node name.
    pub fn local_node_name(&self) -> String {
        self.network.local_node_name()
    }

    /// Listens on a new `Multiaddr`.
    pub fn listen_on(&mut self, addr: Multiaddr) -> impl Stream<Item = ListenerEvent> {
        self.network.listen_on(addr)
    }

    /// Returns the currently active listener addresses.
    pub fn listeners(&self) -> Vec<Multiaddr> {
        self.network.listeners()
    }

    /// Adds an external address.
    pub fn add_external_address(&mut self, addr: Multiaddr) {
        self.network.add_external_address(addr)
    }

    /// Returns the currently used external addresses.
    pub fn external_addresses(&self) -> Vec<AddressRecord> {
        self.network.external_addresses()
    }

    /// Adds a known `Multiaddr` for a `PeerId`.
    pub fn add_address(&mut self, peer: PeerId, addr: Multiaddr) {
        self.network.add_address(peer, addr)
    }

    /// Removes a `Multiaddr` for a `PeerId`.
    pub fn remove_address(&mut self, peer: PeerId, addr: Multiaddr) {
        self.network.remove_address(peer, addr)
    }

    /// Removes all unconnected peers without addresses which have been
    /// in this state for at least the given duration
    pub fn prune_peers(&mut self, min_age: Duration) {
        self.network.prune_peers(min_age);
    }

    /// Dials a `PeerId` using a known address.
    pub fn dial(&mut self, peer: PeerId) {
        self.network.dial(peer);
    }

    /// Dials a `PeerId` using `Multiaddr`.
    pub fn dial_address(&mut self, peer: PeerId, addr: Multiaddr) {
        self.network.dial_address(peer, addr);
    }

    /// Bans a `PeerId` from the swarm, dropping all existing connections and
    /// preventing new connections from the peer.
    pub fn ban(&mut self, peer: PeerId) {
        self.network.ban(peer)
    }

    /// Unbans a previously banned `PeerId`.
    pub fn unban(&mut self, peer: PeerId) {
        self.network.unban(peer)
    }

    /// Returns the known peers.
    pub fn peers(&self) -> Vec<PeerId> {
        self.network.peers()
    }

    /// Returns a list of connected peers.
    pub fn connections(&self) -> Vec<(PeerId, Multiaddr, DateTime<Utc>, Direction)> {
        self.network.connections()
    }

    /// Returns `true` if there is a connection to peer.
    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.network.is_connected(peer)
    }

    /// Returns the `PeerInfo` of a peer.
    pub fn peer_info(&self, peer: &PeerId) -> Option<PeerInfo> {
        self.network.peer_info(peer)
    }

    /// Bootstraps the dht using a set of bootstrap nodes. After bootstrap
    /// completes it provides all blocks in the block store.
    pub fn bootstrap(
        &mut self,
        nodes: Vec<(PeerId, Multiaddr)>,
    ) -> impl Future<Output = Result<()>> {
        self.network.bootstrap(nodes)
    }

    /// Returns true if the dht was bootstrapped.
    pub fn is_bootstrapped(&self) -> bool {
        self.network.is_bootstrapped()
    }

    /// Gets the closest peer to a key. Useful for finding the `Multiaddr` of a
    /// `PeerId`.
    // pub async fn get_closest_peers<K>(&self, key: K) -> Result<()>
    // where
    //     K: Into<BucketKey<K>> + Into<Vec<u8>> + Clone,
    // {
    //     self.network.get_closest_peers(key).await?;
    //     Ok(())
    // }

    /// Gets providers of a key from the dht.
    pub fn providers(&mut self, key: Key) -> impl Future<Output = Result<HashSet<PeerId>>> {
        self.network.providers(key)
    }

    /// Provides a key in the dht.
    pub fn provide(&mut self, key: Key) -> impl Future<Output = Result<()>> {
        self.network.provide(key)
    }

    /// Stops providing a key in the dht.
    pub fn unprovide(&mut self, key: Key) -> Result<()> {
        self.network.unprovide(key)
    }

    /// Gets a record from the dht.
    pub fn get_record(
        &mut self,
        key: Key,
        quorum: Quorum,
    ) -> impl Future<Output = Result<Vec<PeerRecord>>> {
        self.network.get_record(key, quorum)
    }

    /// Puts a new record in the dht.
    pub fn put_record(
        &mut self,
        record: Record,
        quorum: Quorum,
    ) -> impl Future<Output = Result<()>> {
        self.network.put_record(record, quorum)
    }

    /// Removes a record from the dht.
    pub fn remove_record(&mut self, key: Key) -> Result<()> {
        self.network.remove_record(key)
    }

    /// Subscribes to a `topic` returning a `Stream` of messages. If all
    /// `Stream`s for a topic are dropped it unsubscribes from the `topic`.
    pub fn subscribe(
        &mut self,
        topic: String,
    ) -> impl Future<Output = Result<impl Stream<Item = GossipEvent>>> {
        self.network.subscribe(topic)
    }

    /// Publishes a new message in a `topic`, sending the message to all
    /// subscribed peers.
    pub fn publish(&mut self, topic: String, msg: Vec<u8>) -> impl Future<Output = Result<()>> {
        self.network.publish(topic, msg)
    }

    /// Publishes a new message in a `topic`, sending the message to all
    /// subscribed connected peers.
    pub fn broadcast(&mut self, topic: String, msg: Vec<u8>) -> impl Future<Output = Result<()>> {
        self.network.broadcast(topic, msg)
    }

    /// Creates a temporary pin in the block store. A temporary pin is not
    /// persisted to disk and is released once it is dropped.
    pub fn create_temp_pin(&self) -> Result<TempPin> {
        self.storage.create_temp_pin()
    }

    /// Adds a new root to a temporary pin.
    pub fn temp_pin(&self, tmp: &mut TempPin, cid: &Cid) -> Result<()> {
        self.storage.temp_pin(tmp, std::iter::once(*cid))
    }

    /// Returns an `Iterator` of `Cid`s stored in the block store.
    pub fn iter(&self) -> Result<impl Iterator<Item = Cid>> {
        self.storage.iter()
    }

    /// Checks if the block is in the block store.
    pub fn contains(&self, cid: &Cid) -> Result<bool> {
        self.storage.contains(cid)
    }

    /// Returns a block from the block store.
    pub fn get(&self, cid: &Cid) -> Result<Block<P>> {
        if let Some(data) = self.storage.get(cid)? {
            let block = Block::new_unchecked(*cid, data);
            Ok(block)
        } else {
            Err(BlockNotFound(*cid).into())
        }
    }

    /// Either returns a block if it's in the block store or tries to retrieve
    /// it from a peer.
    pub async fn fetch(&self, cid: &Cid, providers: Vec<PeerId>) -> Result<Block<P>> {
        if let Some(data) = self.storage.get(cid)? {
            let block = Block::new_unchecked(*cid, data);
            return Ok(block);
        }
        if !providers.is_empty() {
            self.network.get(*cid, providers).await?.await?;
            if let Some(data) = self.storage.get(cid)? {
                let block = Block::new_unchecked(*cid, data);
                return Ok(block);
            }
            tracing::error!("block evicted too soon. use a temp pin to keep the block around.");
        }
        Err(BlockNotFound(*cid).into())
    }

    /// Inserts a block in to the block store.
    pub fn insert(&self, block: Block<P>) -> Result<()> {
        self.storage.insert(block)?;
        Ok(())
    }

    /// Manually runs garbage collection to completion. This is mainly useful
    /// for testing and administrative interfaces. During normal operation,
    /// the garbage collector automatically runs in the background.
    pub fn evict(&self) -> impl Future<Output = Result<()>> {
        self.storage.evict()
    }

    pub fn sync(
        &self,
        cid: &Cid,
        providers: Vec<PeerId>,
    ) -> impl Future<Output = anyhow::Result<SyncQuery>> {
        let missing = self.storage.missing_blocks(cid).ok().unwrap_or_default();
        tracing::trace!(cid = %cid, missing = %missing.len(), "sync");
        self.network.sync(*cid, providers, missing)
    }

    /// Creates, updates or removes an alias with a new root `Cid`.
    pub fn alias<T: AsRef<[u8]> + Send + Sync>(&self, alias: T, cid: Option<&Cid>) -> Result<()> {
        self.storage.alias(alias.as_ref(), cid)
    }

    /// List all known aliases.
    pub fn aliases(&self) -> Result<Vec<(Vec<u8>, Cid)>> {
        self.storage.aliases()
    }

    /// Returns the root of an alias.
    pub fn resolve<T: AsRef<[u8]> + Send + Sync>(&self, alias: T) -> Result<Option<Cid>> {
        self.storage.resolve(alias.as_ref())
    }

    /// Returns a list of aliases preventing a `Cid` from being garbage
    /// collected.
    pub fn reverse_alias(&self, cid: &Cid) -> Result<Option<HashSet<Vec<u8>>>> {
        self.storage.reverse_alias(cid)
    }

    /// Flushes the block store. After `flush` completes successfully it is
    /// guaranteed that all writes have been persisted to disk.
    pub fn flush(&self) -> impl Future<Output = Result<()>> {
        self.storage.flush()
    }

    /// Perform a set of storage operations in a batch
    ///
    /// The batching concerns only the CacheTracker, it implies no atomicity
    /// guarantees!
    pub fn batch_ops<R>(&self, f: impl FnOnce(&mut Batch<'_, P>) -> Result<R>) -> Result<R> {
        self.storage.rw("batch_ops", f)
    }

    /// Registers prometheus metrics in a registry.
    pub fn register_metrics(&self, registry: &Registry) -> Result<()> {
        self.storage.register_metrics(registry)?;
        net::register_metrics(registry)?;
        Ok(())
    }

    /// Subscribes to the swarm event stream.
    pub fn swarm_events(&mut self) -> impl Future<Output = Result<SwarmEvents>> {
        self.network.swarm_events()
    }
}

#[async_trait]
impl<P: StoreParams> Store for Ipfs<P>
where
    Ipld: References<P::Codecs>,
{
    type Params = P;
    type TempPin = Arc<Mutex<TempPin>>;
    

    fn create_temp_pin(&self) -> Result<Self::TempPin> {
        Ok(Arc::new(Mutex::new(Ipfs::create_temp_pin(self)?)))
    }

    fn temp_pin(&self, tmp: &Self::TempPin, cid: &Cid) -> Result<()> {
        Ipfs::temp_pin(self, &mut tmp.lock(), cid)
    }

    fn contains(&self, cid: &Cid) -> Result<bool> {
        Ipfs::contains(self, cid)
    }

    fn get(&self, cid: &Cid) -> Result<Block<P>> {
        Ipfs::get(self, cid)
    }

    fn insert(&self, block: &Block<P>) -> Result<()> {
        Ipfs::insert(self, block.clone())?;
        Ok(())
    }

    fn alias<T: AsRef<[u8]> + Send + Sync>(&self, alias: T, cid: Option<&Cid>) -> Result<()> {
        Ipfs::alias(self, alias, cid)
    }

    fn resolve<T: AsRef<[u8]> + Send + Sync>(&self, alias: T) -> Result<Option<Cid>> {
        Ipfs::resolve(self, alias)
    }

    fn reverse_alias(&self, cid: &Cid) -> Result<Option<Vec<Vec<u8>>>> {
        Ipfs::reverse_alias(self, cid).map(|x| x.map(|x| x.into_iter().collect()))
    }

    async fn flush(&self) -> Result<()> {
        Ipfs::flush(self).await
    }

    async fn fetch(&self, cid: &Cid) -> Result<Block<Self::Params>> {
        Ipfs::fetch(self, cid, self.peers()).await
    }

    async fn sync(&self, cid: &Cid) -> Result<()> {
        Ipfs::sync(self, cid, self.peers()).await?.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::future::timeout;
    use futures::{join, stream::StreamExt};
    use libipld::{
        alias, cbor::DagCborCodec, ipld, multihash::Code, raw::RawCodec, store::DefaultParams,
    };
    use std::time::Duration;
    use tempdir::TempDir;

    fn tracing_try_init() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init()
            .ok();
    }

    async fn create_store(enable_mdns: bool) -> Result<(Ipfs<DefaultParams>, TempDir)> {
        let tmp = TempDir::new("ipfs-embed")?;
        let sweep_interval = Duration::from_millis(10000);
        let storage = StorageConfig::new(None, None, 10, sweep_interval);

        let mut network = NetworkConfig::new(Keypair::generate());
        if !enable_mdns {
            network.mdns = None;
        }

        let mut ipfs = Ipfs::new(Config { storage, network }).await?;
        ipfs.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
            .next()
            .await
            .unwrap();
        Ok((ipfs, tmp))
    }

    fn create_block(bytes: &[u8]) -> Result<Block<DefaultParams>> {
        Block::encode(RawCodec, Code::Blake3_256, bytes)
    }

    #[async_std::test]
    async fn test_local_store() -> Result<()> {
        tracing_try_init();
        let (store, _tmp) = create_store(false).await?;
        let block = create_block(b"test_local_store")?;
        let mut tmp = store.create_temp_pin()?;
        store.temp_pin(&mut tmp, block.cid())?;
        store.insert(block.clone())?;
        let block2 = store.get(block.cid())?;
        assert_eq!(block.data(), block2.data());
        Ok(())
    }

    #[async_std::test]
    #[ignore] // test is too unreliable for ci
    async fn test_exchange_mdns() -> Result<()> {
        tracing_try_init();
        let (store1, _tmp) = create_store(true).await?;
        let (store2, _tmp) = create_store(true).await?;
        let block = create_block(b"test_exchange_mdns")?;
        let mut tmp1 = store1.create_temp_pin()?;
        store1.temp_pin(&mut tmp1, block.cid())?;
        store1.insert(block.clone())?;
        store1.flush().await?;
        let mut tmp2 = store2.create_temp_pin()?;
        store2.temp_pin(&mut tmp2, block.cid())?;
        let block2 = store2
            .fetch(block.cid(), vec![store1.local_peer_id()])
            .await?;
        assert_eq!(block.data(), block2.data());
        Ok(())
    }

    #[async_std::test]
    #[ignore] // test is too unreliable for ci
    async fn test_exchange_kad() -> Result<()> {
        tracing_try_init();
        let (store, _tmp) = create_store(false).await?;
        let (mut store1, _tmp) = create_store(false).await?;
        let (mut store2, _tmp) = create_store(false).await?;

        let addr = store.listeners()[0].clone();
        let peer_id = store.local_peer_id();
        let nodes = [(peer_id, addr)];

        let b1 = store1.bootstrap(nodes[..].into());
        let b2 = store2.bootstrap(nodes[..].into());
        let (r1, r2) = join!(b1, b2);
        r1.unwrap();
        r2.unwrap();

        let block = create_block(b"test_exchange_kad")?;
        let key = Key::new(&block.cid().to_bytes());
        let mut tmp1 = store1.create_temp_pin()?;
        store1.temp_pin(&mut tmp1, block.cid())?;
        store1.insert(block.clone())?;
        store1.provide(key.clone()).await?;
        store1.flush().await?;

        let mut tmp2 = store2.create_temp_pin()?;
        store2.temp_pin(&mut tmp2, block.cid())?;
        let providers = store2.providers(key).await?;
        let block2 = store2
            .fetch(block.cid(), providers.into_iter().collect())
            .await?;
        assert_eq!(block.data(), block2.data());
        Ok(())
    }

    #[async_std::test]
    async fn test_provider_not_found() -> Result<()> {
        tracing_try_init();
        let (store1, _tmp) = create_store(true).await?;
        let block = create_block(b"test_provider_not_found")?;
        if store1
            .fetch(block.cid(), vec![store1.local_peer_id()])
            .await
            .unwrap_err()
            .downcast_ref::<BlockNotFound>()
            .is_none()
        {
            panic!("expected block not found error");
        }
        Ok(())
    }

    macro_rules! assert_pinned {
        ($store:expr, $block:expr) => {
            assert_eq!(
                $store
                    .reverse_alias($block.cid())
                    .unwrap()
                    .map(|a| !a.is_empty()),
                Some(true)
            );
        };
    }

    macro_rules! assert_unpinned {
        ($store:expr, $block:expr) => {
            assert_eq!(
                $store
                    .reverse_alias($block.cid())
                    .unwrap()
                    .map(|a| !a.is_empty()),
                Some(false)
            );
        };
    }

    fn create_ipld_block(ipld: &Ipld) -> Result<Block<DefaultParams>> {
        Block::encode(DagCborCodec, Code::Blake3_256, ipld)
    }

    #[async_std::test]
    async fn test_sync() -> Result<()> {
        tracing_try_init();
        let (mut local1, _tmp) = create_store(false).await?;
        let (mut local2, _tmp) = create_store(false).await?;
        local1.add_address(local2.local_peer_id(), local2.listeners()[0].clone());
        local2.add_address(local1.local_peer_id(), local1.listeners()[0].clone());

        let a1 = create_ipld_block(&ipld!({ "a": 0 }))?;
        let b1 = create_ipld_block(&ipld!({ "b": 0 }))?;
        let c1 = create_ipld_block(&ipld!({ "c": [a1.cid(), b1.cid()] }))?;
        let b2 = create_ipld_block(&ipld!({ "b": 1 }))?;
        let c2 = create_ipld_block(&ipld!({ "c": [a1.cid(), b2.cid()] }))?;
        let x = alias!(x);

        local1.insert(a1.clone())?;
        local1.insert(b1.clone())?;
        local1.insert(c1.clone())?;
        local1.alias(x, Some(c1.cid()))?;
        local1.flush().await?;
        assert_pinned!(&local1, &a1);
        assert_pinned!(&local1, &b1);
        assert_pinned!(&local1, &c1);

        local2.alias(&x, Some(c1.cid()))?;
        local2
            .sync(c1.cid(), vec![local1.local_peer_id()])
            .await?
            .await?;
        local2.flush().await?;
        assert_pinned!(&local2, &a1);
        assert_pinned!(&local2, &b1);
        assert_pinned!(&local2, &c1);

        local2.insert(b2.clone())?;
        local2.insert(c2.clone())?;
        local2.alias(x, Some(c2.cid()))?;
        local2.flush().await?;
        assert_pinned!(&local2, &a1);
        assert_unpinned!(&local2, &b1);
        assert_unpinned!(&local2, &c1);
        assert_pinned!(&local2, &b2);
        assert_pinned!(&local2, &c2);

        local1.alias(x, Some(c2.cid()))?;
        local1
            .sync(c2.cid(), vec![local2.local_peer_id()])
            .await?
            .await?;
        local1.flush().await?;
        assert_pinned!(&local1, &a1);
        assert_unpinned!(&local1, &b1);
        assert_unpinned!(&local1, &c1);
        assert_pinned!(&local1, &b2);
        assert_pinned!(&local1, &c2);

        local2.alias(x, None)?;
        local2.flush().await?;
        assert_unpinned!(&local2, &a1);
        assert_unpinned!(&local2, &b1);
        assert_unpinned!(&local2, &c1);
        assert_unpinned!(&local2, &b2);
        assert_unpinned!(&local2, &c2);

        local1.alias(x, None)?;
        local2.flush().await?;
        assert_unpinned!(&local1, &a1);
        assert_unpinned!(&local1, &b1);
        assert_unpinned!(&local1, &c1);
        assert_unpinned!(&local1, &b2);
        assert_unpinned!(&local1, &c2);
        Ok(())
    }

    #[async_std::test]
    async fn test_dht_record() -> Result<()> {
        tracing_try_init();
        let mut stores = [create_store(false).await?, create_store(false).await?];
        async_std::task::sleep(Duration::from_millis(100)).await;
        stores[0]
            .0
            .bootstrap(vec![(
                stores[1].0.local_peer_id(),
                stores[1].0.listeners()[0].clone(),
            )])
            .await?;
        stores[1]
            .0
            .bootstrap(vec![(
                stores[0].0.local_peer_id(),
                stores[0].0.listeners()[0].clone(),
            )])
            .await?;

        async_std::task::sleep(Duration::from_millis(500)).await;
        let key: Key = b"key".to_vec().into();

        stores[0]
            .0
            .put_record(
                Record::new(key.clone(), b"hello world".to_vec()),
                Quorum::One,
            )
            .await?;
        let records = stores[1].0.get_record(key, Quorum::One).await?;
        assert_eq!(records.len(), 1);
        Ok(())
    }

    #[async_std::test]
    async fn test_gossip_and_broadcast() -> Result<()> {
        tracing_try_init();
        let mut stores = [
            create_store(false).await?,
            create_store(false).await?,
            create_store(false).await?,
            create_store(false).await?,
            create_store(false).await?,
            create_store(false).await?,
        ];
        let mut subscriptions = vec![];
        let topic = "topic".to_owned();
        let others = stores
            .iter()
            .map(|store| {
                (
                    store.0.local_peer_id(),
                    store.0.listeners().into_iter().next().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        for (store, _) in &mut stores {
            for (peer, addr) in &others {
                if store.local_peer_id() != *peer {
                    store.dial_address(*peer, addr.clone());
                }
            }
        }

        // TCP sim open redials may take a second
        async_std::task::sleep(Duration::from_millis(1500)).await;
        for (store, _) in &stores {
            for (peer, _) in &others {
                assert!(store.is_connected(peer));
            }
        }
        for (store, _) in &mut stores {
            subscriptions.push(store.subscribe(topic.clone()).await?);
        }
        async_std::task::sleep(Duration::from_millis(500)).await;

        stores[0]
            .0
            .publish(topic.clone(), b"hello gossip".to_vec())
            .await
            .unwrap();

        /*
         * This test used to assume that calling subscribe immediately updates the local subscription
         * while sending that subscription over the network takes some time, meaning that all participants
         * received all Subscribed messages. With the new asynchronous NetworkCommand this is no longer
         * true, so Subscribed messages are sometimes missed.
         */
        for (idx, subscription) in subscriptions.iter_mut().enumerate() {
            let mut expected = stores
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    if i == idx {
                        None
                    } else {
                        Some(s.0.local_peer_id())
                    }
                })
                .flat_map(|p| {
                    // once for gossipsub, once for broadcast
                    vec![GossipEvent::Subscribed(p), GossipEvent::Subscribed(p)].into_iter()
                })
                .chain(if idx != 0 {
                    // store 0 is the sender
                    Box::new(std::iter::once(GossipEvent::Message(
                        stores[0].0.local_peer_id(),
                        b"hello gossip".to_vec().into(),
                    ))) as Box<dyn Iterator<Item = GossipEvent>>
                } else {
                    Box::new(std::iter::empty())
                })
                .collect::<Vec<GossipEvent>>();
            while expected
                .iter()
                .any(|msg| matches!(msg, GossipEvent::Message(..)))
            {
                let ev = timeout(Duration::from_millis(100), subscription.next())
                    .await
                    .unwrap_or_else(|_| panic!("idx {} timeout waiting for {:?}", idx, expected))
                    .unwrap();
                assert!(expected.contains(&ev), ", received {:?}", ev);
                if let Some(idx) = expected.iter().position(|e| e == &ev) {
                    // Can't retain, as there might be multiple messages
                    expected.remove(idx);
                }
            }
            if idx != 0 {
                assert!(
                    expected.len() < (stores.len() - 1) * 2,
                    ", idx {} did not receive any Subscribed message",
                    idx
                );
            }
        }

        // Check broadcast subscription
        stores[0]
            .0
            .broadcast(topic.clone(), b"hello broadcast".to_vec())
            .await
            .unwrap();

        for subscription in &mut subscriptions[1..] {
            match subscription.next().await.unwrap() {
                GossipEvent::Message(p, data) => {
                    assert_eq!(p, stores[0].0.local_peer_id());
                    assert_eq!(data[..], b"hello broadcast"[..]);
                }
                x => {
                    panic!("received unexpected message: {:?}", x);
                }
            }
        }

        // trigger cleanup
        let mut last_sub = subscriptions.drain(..1).next().unwrap();
        drop(subscriptions);

        stores[0]
            .0
            .broadcast(topic, b"r u still listening?".to_vec())
            .await
            .unwrap();

        let mut expected = stores[1..]
            .iter()
            .map(|s| s.0.local_peer_id())
            .flat_map(|p| {
                // once for gossipsub, once for broadcast
                vec![GossipEvent::Unsubscribed(p), GossipEvent::Unsubscribed(p)].into_iter()
            })
            .collect::<Vec<_>>();
        while !expected.is_empty() {
            let ev = timeout(Duration::from_millis(100), last_sub.next())
                .await
                .unwrap()
                .unwrap();
            // this is idx==0 which didn’t have a message to receive in the gossipsub round,
            // so the Subscribed messages are still lingering in this channel
            if let GossipEvent::Subscribed(..) = ev {
                continue;
            }
            assert!(expected.contains(&ev), ", received {:?}", ev);
            if let Some(idx) = expected.iter().position(|e| e == &ev) {
                // Can't retain, as there might be multiple messages
                expected.remove(idx);
            }
        }
        Ok(())
    }

    #[async_std::test]
    async fn test_batch_read() -> Result<()> {
        tracing_try_init();
        let network = NetworkConfig::new(Keypair::generate());
        let storage = StorageConfig::new(None, None, 1000000, Duration::from_secs(3600));
        let ipfs = Ipfs::<DefaultParams>::new(Config { storage, network }).await?;
        let a = create_block(b"a")?;
        let b = create_block(b"b")?;
        ipfs.insert(a.clone())?;
        ipfs.insert(b.clone())?;
        let has_blocks = ipfs.batch_ops(|db| Ok(db.contains(a.cid())? && db.contains(b.cid())?))?;
        assert!(has_blocks);
        Ok(())
    }

    #[async_std::test]
    async fn test_batch_write() -> Result<()> {
        tracing_try_init();
        let network = NetworkConfig::new(Keypair::generate());
        let storage = StorageConfig::new(None, None, 1000000, Duration::from_secs(3600));
        let ipfs = Ipfs::<DefaultParams>::new(Config { storage, network }).await?;
        let a = create_block(b"a")?;
        let b = create_block(b"b")?;
        let c = create_block(b"c")?;
        let d = create_block(b"d")?;
        ipfs.batch_ops(|db| {
            db.insert(a.clone())?;
            db.insert(b.clone())?;
            Ok(())
        })?;
        assert!(ipfs.contains(a.cid())? && ipfs.contains(b.cid())?);
        #[allow(unreachable_code)]
        let _: anyhow::Result<()> = ipfs.batch_ops(|db| {
            db.insert(c.clone())?;
            anyhow::bail!("nope!");
            db.insert(d.clone())?;
        });
        assert!(!ipfs.contains(d.cid())? && ipfs.contains(c.cid())? && ipfs.contains(b.cid())?);
        Ok(())
    }

    #[async_std::test]
    #[ignore]
    async fn test_bitswap_sync_chain() -> Result<()> {
        use std::time::Instant;
        tracing_try_init();
        let (a, _tmp) = create_store(true).await?;
        let (b, _tmp) = create_store(true).await?;
        let root = alias!(root);

        let (cid, blocks) = test_util::build_tree(1, 1000)?;
        a.alias(root, Some(&cid))?;
        b.alias(root, Some(&cid))?;

        let size: usize = blocks.iter().map(|block| block.data().len()).sum();
        tracing::info!("chain built {} blocks, {} bytes", blocks.len(), size);
        for block in blocks.iter() {
            a.insert(block.clone())?;
        }
        a.flush().await?;

        let t0 = Instant::now();
        b.sync(&cid, vec![a.local_peer_id()])
            .await?
            .for_each(|x| async move { tracing::debug!("sync progress {:?}", x) })
            .await;
        b.flush().await?;
        tracing::info!(
            "chain sync complete {} ms {} blocks {} bytes!",
            t0.elapsed().as_millis(),
            blocks.len(),
            size
        );
        for block in blocks {
            let data = b.get(block.cid())?;
            assert_eq!(data, block);
        }

        Ok(())
    }

    #[async_std::test]
    #[ignore]
    async fn test_bitswap_sync_tree() -> Result<()> {
        use std::time::Instant;
        tracing_try_init();
        let (a, _tmp) = create_store(true).await?;
        let (b, _tmp) = create_store(true).await?;
        let root = alias!(root);

        let (cid, blocks) = test_util::build_tree(10, 4)?;
        a.alias(root, Some(&cid))?;
        b.alias(root, Some(&cid))?;

        let size: usize = blocks.iter().map(|block| block.data().len()).sum();
        tracing::info!("chain built {} blocks, {} bytes", blocks.len(), size);
        for block in blocks.iter() {
            a.insert(block.clone())?;
        }
        a.flush().await?;

        let t0 = Instant::now();
        b.sync(&cid, vec![a.local_peer_id()])
            .await?
            .for_each(|x| async move { tracing::debug!("sync progress {:?}", x) })
            .await;
        b.flush().await?;
        tracing::info!(
            "tree sync complete {} ms {} blocks {} bytes!",
            t0.elapsed().as_millis(),
            blocks.len(),
            size
        );
        for block in blocks {
            let data = b.get(block.cid())?;
            assert_eq!(data, block);
        }
        Ok(())
    }
}
