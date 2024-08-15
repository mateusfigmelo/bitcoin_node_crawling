#![allow(unused)]
mod codec;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
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

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The address of the node to reach to.
    /// `dig seed.bitcoin.sipa.be +short` may provide a fresh list of nodes.
    #[arg(short, long, default_value = "47.243.121.223:8333")]
    remote_address: String,

    /// The connection timeout, in milliseconds.
    /// Most nodes will quickly respond. If they don't, we'll probably want to talk to other nodes instead.
    #[arg(short, long, default_value = "500")]
    connection_timeout_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();

    let remote_address = args
        .remote_address
        .parse::<SocketAddr>()
        .context("Invalid remote address")?;
    let mut stream: Framed<TcpStream, BitcoinCodec> =
        connect(&remote_address, args.connection_timeout_ms).await?;

    Ok(perform_handshake(&mut stream, remote_address).await?)
}

async fn connect(
    remote_address: &SocketAddr,
    connection_timeout: u64,
) -> Result<Framed<TcpStream, BitcoinCodec>, Error> {
    let connection = TcpStream::connect(remote_address).map_err(Error::ConnectionFailed);
    let stream = timeout(Duration::from_millis(connection_timeout), connection)
        .map_err(Error::ConnectionTimedOut)
        .await??;
    let framed = Framed::new(stream, BitcoinCodec {});
    Ok(framed)
}

/// Perform a Bitcoin handshake as per [this protocol documentation](https://en.bitcoin.it/wiki/Protocol_documentation)
async fn perform_handshake(
    stream: &mut Framed<TcpStream, BitcoinCodec>,
    peer_address: SocketAddr,
) -> Result<(), Error> {
    let version_message = RawNetworkMessage::new(
        Network::Bitcoin.magic(),
        NetworkMessage::Version(build_version_message(&peer_address)),
    );

    stream
        .send(version_message)
        .await
        .map_err(Error::SendingFailed)?;

    while let Some(result) = stream.next().await {
        match result {
            Ok(message) => match message.payload() {
                NetworkMessage::Version(remote_version) => {
                    tracing::info!("Version message: {:?}", remote_version);

                    stream
                        .send(RawNetworkMessage::new(
                            Network::Bitcoin.magic(),
                            NetworkMessage::Verack,
                        ))
                        .await
                        .map_err(Error::SendingFailed)?;

                    return Ok(());
                }
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

    let env = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    let fmt_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(false)
        .with_target(false);
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(env)
        .init();
}
