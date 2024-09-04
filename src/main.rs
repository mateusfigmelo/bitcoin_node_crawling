#![allow(unused)]
mod codec;

use std::collections::{HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, IpAddr};
use std::time::Duration;

use anyhow::Context;
use bitcoin::p2p::message::{NetworkMessage, RawNetworkMessage};
use bitcoin::p2p::message_network::VersionMessage;
use bitcoin::p2p::{Address, ServiceFlags};
use bitcoin::Network;
use clap::Parser;
use codec::BitcoinCodec;
use futures::{SinkExt, StreamExt, TryFutureExt};
use rand::Rng;
use tokio::net::TcpStream;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use std::sync::Arc;
use tokio::sync::Semaphore;


#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The address of the node to reach to.
    /// `dig seed.bitcoin.sipa.be +short` may provide a fresh list of nodes.
    #[arg(short, long, default_value = "47.243.121.223:8333")]
    remote_address: String,

    /// The connection timeout, in milliseconds.
    /// Most nodes will quickly respond. If they don't, we'll probably want to talk to other nodes instead.
    #[arg(short, long, default_value = "300")]
    connection_timeout_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    init_tracing();
    // Parse command-line arguments
    let args = Args::parse();
    // Parse the remote address
    let remote_address = args
        .remote_address
        .parse::<SocketAddr>()
        .context("Invalid remote address")?;
    // Connect to the remote address
    let peer_addresses = crawl_network(
        vec![remote_address].into_iter().collect(),
        args.connection_timeout_ms,
    ).await?;
    // Print the list of nodes
    println!("\n*********The List of Nodes***********");
    println!("Total Nodes: {}", peer_addresses.len());
    println!("-------------------------------------");
    println!("{:?}",peer_addresses);
    Ok(())
}

// Placeholder function to check if an address is IPv6
fn is_ipv6(addr: &SocketAddr) -> bool {
    matches!(addr.ip(), IpAddr::V6(_))
}

// Placeholder function to check if an address is a Tor address
fn is_tor(addr: &SocketAddr) -> bool {
    // In a real scenario, implement your own logic to identify Tor addresses
    // Here we're assuming some custom check, like checking domain names
    false // Placeholder logic
}

async fn connect(
    remote_address: &SocketAddr,
    connection_timeout: u64,
) -> Result<Framed<TcpStream, BitcoinCodec>, Error> {
    // Connect to the remote address
    let connection = TcpStream::connect(remote_address).map_err(Error::ConnectionFailed);
    // Set a timeout for the connection
    let stream = timeout(Duration::from_millis(connection_timeout), connection)
        .map_err(Error::ConnectionTimedOut)
        .await??;
    // Create a framed stream
    let framed = Framed::new(stream, BitcoinCodec {});
    Ok(framed)
}

/// Perform a Bitcoin handshake as per [this protocol documentation](https://en.bitcoin.it/wiki/Protocol_documentation)
async fn perform_handshake_getaddr(
    stream: &mut Framed<TcpStream, BitcoinCodec>,
    address: SocketAddr,
) -> Result<HashSet<SocketAddr>, Error> {
    let version_message = RawNetworkMessage::new(
        Network::Bitcoin.magic(),
        NetworkMessage::Version(build_version_message(&address)),
    );
    // Send the version message
    stream
        .send(version_message)
        .await
        .map_err(Error::SendingFailed)?;

    //Process incoming messages
    while let Some(result) = stream.next().await {
        match result {
            Ok(message) => match message.payload() {
                // Handle the version message
                NetworkMessage::Version(remote_version) => {
                    tracing::info!("version message received: {:?}", address);
                    stream
                        .send(RawNetworkMessage::new(
                            Network::Bitcoin.magic(),
                            NetworkMessage::Verack,
                        ))
                        .await
                        .map_err(Error::SendingFailed)?;
                }
                // Handle the verack message
                NetworkMessage::Verack => {
                    tracing::info!("verack message received: {:?}", address);
                    stream
                        .send(RawNetworkMessage::new(
                            Network::Bitcoin.magic(),
                            NetworkMessage::GetAddr,
                        ))
                        .await
                        .map_err(Error::SendingFailed)?;
                }
                // Handle the addr message
                NetworkMessage::Addr(addresses) => {
                    tracing::info!("addr message received: {:?}", address);
                    let socket_addresses: HashSet<SocketAddr> = addresses
                        .iter()
                        .filter_map(|(_, address)| address.socket_addr().ok())
                        .collect();
                    return Ok(socket_addresses);
                }
                // Handle other messages
                other_message => {
                    // We're only interested in the version message right now. Keep the loop running.
                    tracing::debug!("Unsupported message: {:?}", other_message);
                }
            },
            Err(err) => {
                tracing::error!("Decoding error: {}", err);
            }
        }
    }
    Err(Error::ConnectionLost)
}

/// Crawl the Bitcoin network by connecting to a set of seed addresses and asking for more addresses.
async fn crawl_network(
    seed_addresses: HashSet<SocketAddr>,
    connection_timeout: u64,
) -> Result<HashSet<SocketAddr>, Error> {

    let max_concurrent_connections = 5000; // Increased concurrency
    let max_peers = 5000;    // Target number of peers
    
    // Flag to stop crawling the network
    let mut flag_done=false;
    let semaphore = Arc::new(Semaphore::new(max_concurrent_connections)); // Semaphore to limit concurrent connections
    let mut known_addresses: HashSet<SocketAddr> = HashSet::new(); // Addresses we already know
    let mut visited_addresses: HashSet<SocketAddr> = HashSet::new(); // Addresses we have already visited
    let mut pending_addresses: VecDeque<HashSet<SocketAddr>> = VecDeque::new(); // Addresses we have discovered but not yet visited
    pending_addresses.push_back(seed_addresses.clone()); // Seed addresses to start crawling

    for initial_address in seed_addresses{
        known_addresses.insert(initial_address); // Seed addresses are known
    }
    
    // Start crawling the network
    while !pending_addresses.is_empty() {
        // Pop the first set of addresses from the queue
        let addresses = pending_addresses.pop_front().unwrap();
        // Create a task for each address
        let mut tasks = Vec::new();
        // Visit each address
        for addr in addresses {
            // Skip if we have already visited this address
            if visited_addresses.contains(&addr) {
                continue;
            }
            // Mark the address as visited
            visited_addresses.insert(addr);
            // Clone the semaphore to pass it to the task
            let semaphore: Arc<Semaphore> = Arc::clone(&semaphore);
            // Create a task to connect to the address
            let task = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();
                match timeout(Duration::from_millis(connection_timeout), connect(&addr, connection_timeout)).await {
                    Ok(Ok(mut stream)) => {
                        if let Ok(new_addresses) = perform_handshake_getaddr(&mut stream, addr).await {
                            return Some((addr, new_addresses));
                        }
                    }
                    Ok(Err(err)) => {
                        tracing::error!("Connection to {} failed: {}", addr, err);
                    }
                    _ => {}
                }
                None
            });
            tasks.push(task);
        }

        // Wait for all tasks to complete
        let results = futures::future::join_all(tasks).await;
        for result in results {
            if let Ok(Some((addr, new_addresses))) = result {
                let mut filtered_addresses = HashSet::new();
                for new_addr in new_addresses {
                    if is_ipv6(&new_addr) || is_tor(&new_addr) {
                        continue; // Skip IPv6 or Tor addresses
                    }
                    // Add the new address to the list of known addresses
                    if !known_addresses.contains(&new_addr) {
                        filtered_addresses.insert(new_addr);
                        tracing::info!("New address found: {}", new_addr);
                        known_addresses.insert(new_addr);
                    }
                    // Stop crawling if we have reached the target number of peers
                    if known_addresses.len() >= max_peers {
                        flag_done=true;
                        break;
                    }
                }
                // Add the new addresses to the queue
                if !filtered_addresses.is_empty() {
                    pending_addresses.push_back(filtered_addresses);
                }
            } 
            // Stop crawling if we have reached the target number of peers
            if flag_done {
                break;
            }
        }
        // Stop crawling if we have reached the target number of peers
        if flag_done {
            break;
        }
    }
    // Return the list of known addresses
    Ok(known_addresses)
}


#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Connection failed: {0:?}")]
    ConnectionFailed(std::io::Error),
    #[error("Connection timed out")]
    ConnectionTimedOut(Elapsed),
    #[error("Connection lost")]
    ConnectionLost,
    #[error("Sending failed")]
    SendingFailed(std::io::Error),
}

fn build_version_message(receiver_address: &SocketAddr) -> VersionMessage {
    /// The height of the block that the node is currently at.
    /// We are always at the genesis block. because our implementation is not a real node.
    const START_HEIGHT: i32 = 0;
    /// The most popular user agent. See https://bitnodes.io/nodes/
    const USER_AGENT: &str = "/Satoshi:25.0.0/";
    const SERVICES: ServiceFlags = ServiceFlags::NONE;
    /// The address of this local node.
    /// This address doesn't matter much as it will be ignored by the bitcoind node in most cases.
    let sender_address: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0));

    let sender = Address::new(&sender_address, SERVICES);
    let timestamp = chrono::Utc::now().timestamp();
    let receiver = Address::new(&receiver_address, SERVICES);
    let nonce = rand::thread_rng().gen();
    let user_agent = USER_AGENT.to_string();
    // Create a version message
    VersionMessage::new(
        SERVICES,
        timestamp,
        receiver,
        sender,
        nonce,
        user_agent,
        START_HEIGHT,
    )
}

pub fn init_tracing() {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;
    // Set up tracing
    let env = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("RUST_LOG")
        .from_env_lossy();
    // Set up formatting
    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(false)
        .with_target(false);
    // Initialize the subscriber
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(env)
        .init();
}
