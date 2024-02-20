use crate::utils::build_swarm;
use codec::Decode;
use futures::FutureExt;
use futures::StreamExt;
use jsonrpsee::{
    client_transport::ws::{Url, WsTransportClientBuilder},
    core::client::{Client, ClientT},
    rpc_params,
};
use libp2p::{
    identify::Info,
    identity::Keypair,
    kad::{record::Key as KademliaKey, GetRecordOk, KademliaEvent, QueryId, QueryResult},
    multiaddr,
    swarm::{DialError, SwarmEvent},
    Multiaddr, PeerId, Swarm,
};
use multihash_codetable::{Code, MultihashDigest};
use prost::Message;
use rand::{seq::SliceRandom, thread_rng};
use std::collections::{HashMap, HashSet};
use subp2p_explorer::{
    peer_behavior::PeerInfoEvent,
    transport::{TransportBuilder, MIB},
    Behaviour, BehaviourEvent,
};

const _POLKADOT_URL: &str = "wss://rpc.polkadot.io:443";

pub async fn client(url: Url) -> Result<Client, Box<dyn std::error::Error>> {
    let (sender, receiver) = WsTransportClientBuilder::default().build(url).await?;

    Ok(Client::builder()
        .max_buffer_capacity_per_subscription(4096)
        .build_with_tokio(sender, receiver))
}

mod sr25519 {
    /// Public key for sr25519 keypair implementation.
    pub type PublicKey = [u8; 32];

    /// Signature generated by signing.
    pub type Signature = [u8; 64];

    /// Verify that the signature of the message was generated with the public key.
    pub fn verify<M: AsRef<[u8]>>(sig: &Signature, message: M, pubkey: &PublicKey) -> bool {
        const SIGNING_CTX: &[u8] = b"substrate";

        let Ok(signature) = schnorrkel::Signature::from_bytes(sig) else {
            return false;
        };
        let Ok(public) = schnorrkel::PublicKey::from_bytes(pubkey) else {
            return false;
        };

        public
            .verify_simple(SIGNING_CTX, message.as_ref(), &signature)
            .is_ok()
    }
}

async fn runtime_api_autorities(
    url: Url,
) -> Result<Vec<sr25519::PublicKey>, Box<dyn std::error::Error>> {
    let client = client(url).await?;

    // State call provides the result hex-encoded.
    let raw: String = client
        .request(
            "state_call",
            rpc_params!["AuthorityDiscoveryApi_authorities", "0x"],
        )
        .await?;
    let raw = raw
        .strip_prefix("0x")
        .expect("Substrate API returned invalid hex");

    let bytes = hex::decode(&raw)?;

    let authorities: Vec<sr25519::PublicKey> = Decode::decode(&mut &bytes[..])?;
    Ok(authorities)
}

fn hash_authority_id(id: &[u8]) -> KademliaKey {
    KademliaKey::new(&Code::Sha2_256.digest(id).digest())
}

// At most MAX_QUERIES queries at a time.
const MAX_QUERIES: usize = 16;

mod schema {
    include!(concat!(env!("OUT_DIR"), "/authority_discovery_v2.rs"));
}

fn get_peer_id(address: &Multiaddr) -> Option<PeerId> {
    match address.iter().last() {
        Some(multiaddr::Protocol::P2p(key)) => Some(key),
        _ => None,
    }
}

fn decode_dht_record(
    value: Vec<u8>,
    authority_id: &sr25519::PublicKey,
) -> Result<(PeerId, Vec<Multiaddr>), Box<dyn std::error::Error>> {
    // Decode and verify the authority signature.
    let payload = schema::SignedAuthorityRecord::decode(value.as_slice())?;
    let auth_signature = sr25519::Signature::decode(&mut &payload.auth_signature[..])?;
    if !sr25519::verify(&auth_signature, &payload.record, &authority_id) {
        return Err("Cannot verify DHT payload".into());
    }

    // Extract the P2P multiaddresses from the prvoided record.
    let record = schema::AuthorityRecord::decode(payload.record.as_slice())?;
    let addresses: Vec<Multiaddr> = record
        .addresses
        .into_iter()
        .map(|a| a.try_into())
        .collect::<std::result::Result<_, _>>()?;

    // At least one address must be provided and all must point to the same peerId.
    if addresses.is_empty() {
        return Err("No addresses found in the DHT record".into());
    }
    let peer_ids: HashSet<_> = addresses.iter().filter_map(get_peer_id).collect();
    if peer_ids.len() != 1 {
        return Err(format!(
            "All addresses must point to the same peerId: {:?}",
            addresses
        )
        .into());
    }

    let peer_id = peer_ids
        .iter()
        .next()
        .expect("At least one peerId; qed")
        .clone();

    // Verify peer signature.
    let Some(peer_signature) = payload.peer_signature else {
        return Err("Payload is not signed".into());
    };
    let public_key = libp2p::identity::PublicKey::try_decode_protobuf(&peer_signature.public_key)?;
    if peer_id != public_key.to_peer_id() {
        return Err("PeerId does not match the public key".into());
    }
    if !public_key.verify(&payload.record.as_slice(), &peer_signature.signature) {
        return Err("Peer signature verification failed".into());
    }

    Ok((peer_id, addresses))
}

struct AuthorityDiscovery {
    /// Drive the network behavior.
    swarm: Swarm<Behaviour>,
    /// In flight kademlia queries.
    queries: HashMap<QueryId, sr25519::PublicKey>,
    /// Kademlia queries might produce multiple results for the same entry.
    permanent_queries: HashMap<QueryId, sr25519::PublicKey>,

    /// Map the in-flight kademlia queries to the authority ids.
    records_keys: HashMap<KademliaKey, sr25519::PublicKey>,

    /// In flight kademlia queries.
    queries_discovery: HashSet<QueryId>,
    /// Peer details including protocols, multiaddress from the identify protocol.
    peer_info: HashMap<PeerId, Info>,
    /// Peer details obtained from the DHT.
    peer_details: HashMap<PeerId, PeerDetails>,

    authority_to_details: HashMap<sr25519::PublicKey, HashSet<Multiaddr>>,

    /// Provided authority list.
    authorities: Vec<sr25519::PublicKey>,
    /// Query index.
    query_index: usize,

    /// Encountered DHT errors, either from decoding or protocol transport.
    dht_errors: usize,
    /// Remaining authorities to query.
    remaining_authorities: HashSet<sr25519::PublicKey>,
    /// Finished DHT queries for authority records.
    finished_query: bool,

    /// Interval at which to resubmit the remaining queries.
    interval: tokio::time::Interval,
    /// Interval at which to bail out.
    interval_exit: tokio::time::Interval,
}

#[derive(Clone)]
struct PeerDetails {
    /// Authority ID from the runtime API.
    authority_id: sr25519::PublicKey,
    /// Discovered from the DHT.
    addresses: HashSet<Multiaddr>,
}

impl AuthorityDiscovery {
    pub fn new(swarm: Swarm<Behaviour>, authorities: Vec<sr25519::PublicKey>) -> Self {
        AuthorityDiscovery {
            swarm,
            queries: HashMap::with_capacity(1024),
            permanent_queries: HashMap::with_capacity(1024),

            records_keys: HashMap::with_capacity(1024),

            queries_discovery: HashSet::with_capacity(1024),
            peer_info: HashMap::with_capacity(1024),
            peer_details: HashMap::with_capacity(1024),
            authority_to_details: HashMap::with_capacity(1024),

            authorities: authorities.clone(),
            query_index: 0,

            dht_errors: 0,
            remaining_authorities: authorities.into_iter().collect(),
            finished_query: false,
            interval: tokio::time::interval(std::time::Duration::from_secs(60)),
            interval_exit: tokio::time::interval(std::time::Duration::from_secs(2 * 60 + 30)),
        }
    }

    fn query_dht_records(&mut self, authorities: impl IntoIterator<Item = sr25519::PublicKey>) {
        // Make a query for every authority.
        for authority in authorities {
            let key = hash_authority_id(&authority);
            self.records_keys.insert(key.clone(), authority);

            let id = self.swarm.behaviour_mut().discovery.get_record(key);
            self.queries.insert(id, authority.clone());
            self.permanent_queries.insert(id, authority.clone());
        }
    }

    fn query_peer_info(&mut self) {
        const MAX_QUERIES: usize = 128;

        let peers = self.peer_details.keys().cloned().filter_map(|peer| {
            if self.peer_info.contains_key(&peer) {
                None
            } else {
                Some(peer)
            }
        });

        if self.queries_discovery.len() < MAX_QUERIES {
            let query_num = MAX_QUERIES - self.queries_discovery.len();
            for peer in peers.take(query_num) {
                self.queries_discovery
                    .insert(self.swarm.behaviour_mut().discovery.get_closest_peers(peer));
            }
        }
    }

    fn advanced_dht_queries(&mut self) {
        // Add more DHT queries.
        while self.queries.len() < MAX_QUERIES {
            if let Some(next) = self.authorities.get(self.query_index) {
                self.query_dht_records(std::iter::once(next.clone()));
                self.query_index += 1;
            } else {
                println!(
                    "queries: {} remaining {}",
                    self.queries.len(),
                    self.remaining_authorities.len()
                );

                break;
            }
        }
    }

    fn resubmit_remaining_dht_queries(&mut self) {
        // Ignore older queries.
        self.queries = HashMap::with_capacity(1024);

        let authorities = self.remaining_authorities.clone();
        let mut remaining: Vec<_> = authorities.iter().collect();
        remaining.shuffle(&mut thread_rng());

        println!(
            " Remaining authorities: {}",
            self.remaining_authorities.len()
        );

        self.query_dht_records(remaining.into_iter().take(MAX_QUERIES).cloned());
    }

    fn handle_swarm<T>(&mut self, event: SwarmEvent<BehaviourEvent, T>) {
        match event {
            // Discovery DHT record.
            SwarmEvent::Behaviour(behavior_event) => {
                match behavior_event {
                    BehaviourEvent::Discovery(KademliaEvent::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetRecord(record),
                        ..
                    }) => {
                        // Has received at least one answer for this.
                        self.queries.remove(&id);

                        match record {
                            Ok(GetRecordOk::FoundRecord(peer_record)) => {
                                let key = peer_record.record.key;
                                let value = peer_record.record.value;

                                let Some(authority) = self.records_keys.remove(&key) else {
                                    return;
                                };

                                let (peer_id, addresses) =
                                    match decode_dht_record(value, &authority) {
                                        Ok((peer_id, addresses)) => (peer_id, addresses),
                                        Err(e) => {
                                            println!(
                                                " Decoding DHT failed for authority {:?}: {:?}",
                                                authority, e
                                            );
                                            self.dht_errors += 1;
                                            return;
                                        }
                                    };

                                self.authority_to_details
                                    .entry(authority)
                                    .and_modify(|entry| entry.extend(addresses.clone()))
                                    .or_insert_with(|| addresses.iter().cloned().collect());

                                self.peer_details
                                    .entry(peer_id)
                                    .and_modify(|entry| entry.addresses.extend(addresses.clone()))
                                    .or_insert_with(|| PeerDetails {
                                        authority_id: authority,
                                        addresses: addresses.iter().cloned().collect(),
                                    });

                                println!(
                                    "{}/{} (err {}) authority: {:?} peer_id {:?} Addresses: {:?}",
                                    self.authority_to_details.len(),
                                    self.authorities.len(),
                                    self.dht_errors,
                                    authority,
                                    peer_id,
                                    addresses
                                );

                                self.remaining_authorities.remove(&authority);
                                self.advanced_dht_queries();

                                if self.peer_details.len() == self.authorities.len() {
                                    println!("All authorities discovered from DHT: Expected {} Errors {}", self.authorities.len(), self.dht_errors);

                                    let discovered = self
                                        .peer_details
                                        .keys()
                                        .filter_map(|peer| self.peer_info.get(peer))
                                        .count();
                                    println!(
                                        "Fully discovered at the moment {}/{}",
                                        discovered,
                                        self.authorities.len()
                                    );

                                    for peer in self.peer_details.keys() {
                                        if self.peer_info.contains_key(peer) {
                                            let _ = self.swarm.disconnect_peer_id(peer.clone());
                                        }
                                    }

                                    self.query_peer_info();
                                    self.finished_query = true;
                                }
                            }
                            _ => (),
                        }
                    }

                    BehaviourEvent::Discovery(KademliaEvent::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetClosestPeers(_),
                        ..
                    }) => {
                        if self.finished_query {
                            println!(" Discovered closes peers of {:?}", id);
                        }

                        self.queries_discovery.remove(&id);
                        self.query_peer_info();
                    }

                    BehaviourEvent::PeerInfo(info_event) => {
                        match info_event {
                            PeerInfoEvent::Identified { peer_id, info } => {
                                if self.finished_query {
                                    let discovered = self
                                        .peer_details
                                        .keys()
                                        .filter_map(|peer| self.peer_info.get(peer))
                                        .count();

                                    println!(
                                        " {}/{} Info event {:?}",
                                        discovered,
                                        self.authorities.len(),
                                        peer_id
                                    );
                                }

                                // Save the record.
                                self.peer_info.insert(peer_id, info);
                            }
                        };
                    }
                    _ => (),
                }
            }

            _ => (),
        }
    }

    pub async fn discover(&mut self) {
        self.advanced_dht_queries();

        // Should return immediately.
        self.interval.tick().await;
        self.interval_exit.tick().await;

        loop {
            if self.authority_to_details.len() == self.authorities.len() {
                println!("All authorities discovered from DHT");
                break;
            }

            futures::select! {
                event = self.swarm.select_next_some().fuse() => {
                    self.handle_swarm(event);
                },

                _ = self.interval.tick().fuse() => {
                    self.resubmit_remaining_dht_queries();
                }

                _ = self.interval_exit.tick().fuse() => {
                    println!("Exiting due to timeout");
                    return;
                }
            }
        }
    }
}

/// Reach a single peer and query the identify protocol.
///
/// # Example
///
/// The following address is taken from the DHT.
/// However, the address cannot be reached directly.
/// For this to work, we'd need to implement NAT hole punching.
///
/// ```rust
/// let addr =
///     "/ip4/34.92.86.244/tcp/40333/p2p/12D3KooWKxsprneVYQxxPnPUwDA5p2huuCbZCNyuSHTmKDv3vT2n";
/// let addr: Multiaddr = addr.parse().expect("Valid multiaddress; qed");
/// let peer_id = get_peer_id(&addr);
/// let info = PeerInfo::new(local_key.clone(), vec![addr]);
/// let info = info.discover().await;
/// println!("Peer={:?} version={:?}", peer_id, info);
/// ```
struct PeerInfo {
    swarm: Swarm<libp2p::identify::Behaviour>,
}

impl PeerInfo {
    pub fn new(local_key: Keypair, addresses: Vec<Multiaddr>) -> Self {
        // "/ip4/144.76.115.244/tcp/30333/p2p/12D3KooWKR7TX55EnZ6L6FUHfuZKAEgkL8ffE3KFYqnHZUysSVrW"
        let mut swarm: Swarm<libp2p::identify::Behaviour> = {
            let transport = TransportBuilder::new()
                .yamux_maximum_buffer_size(256 * MIB)
                .build(local_key.clone());

            let identify_config =
                libp2p::identify::Config::new("/substrate/1.0".to_string(), local_key.public())
                    .with_agent_version("subp2p-identify".to_string())
                    // Do not cache peer info.
                    .with_cache_size(0);
            let identify = libp2p::identify::Behaviour::new(identify_config);

            let local_peer_id = PeerId::from(local_key.public());
            libp2p::swarm::SwarmBuilder::with_tokio_executor(transport, identify, local_peer_id)
                .build()
        };

        // These are the initial peers for which the queries are performed against.
        for multiaddress in &addresses {
            let res = swarm.dial(multiaddress.clone());
        }

        PeerInfo { swarm }
    }

    pub async fn discover(mut self) -> Result<Info, DialError> {
        loop {
            let event = self.swarm.select_next_some().await;

            match event {
                SwarmEvent::Behaviour(behavior) => match behavior {
                    libp2p::identify::Event::Received { info, .. } => {
                        return Ok(info);
                    }
                    _ => (),
                },

                SwarmEvent::OutgoingConnectionError { error, .. } => return Err(error),

                _ => (),
            }
        }
    }
}

enum VersionRegistry {
    Polkadot,
    Substrate,
    Kusama,
}

impl VersionRegistry {
    pub fn to_version(self) -> u16 {
        match self {
            VersionRegistry::Polkadot => 0,
            VersionRegistry::Substrate => 42,
            VersionRegistry::Kusama => 2,
        }
    }
}

fn ss58hash(data: &[u8]) -> Vec<u8> {
    use blake2::{Blake2b512, Digest};
    const PREFIX: &[u8] = b"SS58PRE";

    let mut ctx = Blake2b512::new();
    ctx.update(PREFIX);
    ctx.update(data);
    ctx.finalize().to_vec()
}

fn to_ss58(key: &sr25519::PublicKey, version: VersionRegistry) -> String {
    // We mask out the upper two bits of the ident - SS58 Prefix currently only supports 14-bits
    let ident: u16 = version.to_version() & 0b0011_1111_1111_1111;
    let mut v = match ident {
        0..=63 => vec![ident as u8],
        64..=16_383 => {
            // upper six bits of the lower byte(!)
            let first = ((ident & 0b0000_0000_1111_1100) as u8) >> 2;
            // lower two bits of the lower byte in the high pos,
            // lower bits of the upper byte in the low pos
            let second = ((ident >> 8) as u8) | ((ident & 0b0000_0000_0000_0011) as u8) << 6;
            vec![first | 0b01000000, second]
        }
        _ => unreachable!("masked out the upper two bits; qed"),
    };
    v.extend(key.as_ref());
    let r = ss58hash(&v);
    v.extend(&r[0..2]);
    bs58::encode(v).into_string()
}

pub async fn discover_authorities(
    url: String,
    genesis: String,
    bootnodes: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse(&url)?;

    // Extract the authorities from the runtime API.
    let authorities = runtime_api_autorities(url).await?;

    // Perform DHT queries to find the authorities on the network.
    // Then, record the addresses of the authorities and the responses
    // from the identify protocol.
    let swarm = build_swarm(genesis.clone(), bootnodes)?;
    let mut authority_discovery = AuthorityDiscovery::new(swarm, authorities.clone());
    authority_discovery.discover().await;

    println!("Finished discovery\n");

    let mut reached_peers = 0;

    for authority in &authorities {
        let Some(details) = authority_discovery.authority_to_details.get(authority) else {
            println!(
                "authority={:?} - No dht response",
                to_ss58(authority, VersionRegistry::Polkadot),
            );
            continue;
        };

        let Some(addr) = details.iter().next() else {
            println!(
                "authority={:?} - No addresses found in DHT record",
                to_ss58(authority, VersionRegistry::Polkadot),
            );
            continue;
        };

        let peer_id = get_peer_id(addr).expect("All must have valid peerIDs");

        let info = authority_discovery.peer_info.get(&peer_id).cloned();
        if let Some(info) = info {
            reached_peers += 1;

            println!(
                "authority={:?} peer_id={:?} addresses={:?} version={:?} ",
                to_ss58(authority, VersionRegistry::Polkadot),
                peer_id,
                info.agent_version,
                details,
            );
        } else {
            println!(
                "authority={:?} peer_id={:?} addresses={:?} - Cannot be reached",
                to_ss58(authority, VersionRegistry::Polkadot),
                peer_id,
                details,
            );
        }
    }

    println!(
        "\n\n  Discovered {}/{} authorities",
        reached_peers,
        authorities.len()
    );

    Ok(())
}
