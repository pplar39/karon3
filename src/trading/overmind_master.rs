use crate::trading::overmind_protocol::{Envelope, OvermindSignal, MAX_PACKET_SIZE};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub struct OvermindMaster {
    socket: UdpSocket,
    // List of Edge UDP addresses (IP:Port)
    edge_nodes: Vec<String>,
    hmac_key: [u8; 32],
    // Seq counter
    seq: std::sync::atomic::AtomicU64,
}

impl OvermindMaster {
    pub async fn new(bind_addr: &str, edge_nodes: Vec<String>, hmac_key_str: &str) -> Result<Self> {
        let socket = UdpSocket::bind(bind_addr)
            .await
            .context("Failed to bind Master UDP socket")?;
        socket
            .set_broadcast(true)
            .context("Failed to enable broadcast")?;

        info!("[OVERMIND-MASTER] Online at {}", bind_addr);
        info!("[OVERMIND-MASTER] Registered Edge Nodes: {:?}", edge_nodes);

        let mut hmac_key = [0u8; 32];
        let key_bytes = bs58::decode(hmac_key_str)
            .into_vec()
            .context("Invalid HMAC Key")?;
        if key_bytes.len() != 32 {
            anyhow::bail!("HMAC Key must be 32 bytes");
        }
        hmac_key.copy_from_slice(&key_bytes);

        Ok(Self {
            socket,
            edge_nodes,
            hmac_key,
            seq: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub async fn broadcast_signal(&self, signal: OvermindSignal) -> Result<()> {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts_ns = crate::time::monotonic_now_ns();
        let nonce = rand::random::<u64>();

        let mut envelope = Envelope::new(
            seq,
            ts_ns,
            nonce,
            "MASTER".to_string(),
            signal.clone(),
            &self.hmac_key,
        )?;

        let payload = bincode::serialize(&envelope)?;

        if payload.len() > MAX_PACKET_SIZE {
            anyhow::bail!("Packet too large: {} bytes", payload.len());
        }

        for node in &self.edge_nodes {
            match self.socket.send_to(&payload, node).await {
                Ok(_) => info!(
                    "[OVERMIND-SIGNAL] Sent Seq:{} Type:{:?} to {}",
                    seq, signal, node
                ),
                Err(e) => error!("[OVERMIND-SIGNAL] Failed to send to {}: {}", node, e),
            }
        }
        Ok(())
    }

    // "The Telepathy"
    pub async fn trigger_sell_all(&self, token: &Option<String>) -> Result<()> {
        info!(
            "[OVERMIND] >>> TELEPATHIC SELL SIGNAL TRIGGERED (Target: {:?})",
            token
        );
        let signal = OvermindSignal::SellAll {
            token: token.clone(),
        };
        self.broadcast_signal(signal).await
    }

    // "The Logistics"
    pub async fn trigger_sweep(&self) -> Result<()> {
        info!("[OVERMIND] >>> ASYNC CONSOLIDATION TRIGGERED");
        let signal = OvermindSignal::Sweep;
        self.broadcast_signal(signal).await
    }
}
