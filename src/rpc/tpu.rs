//! Direct TPU sender for faster transaction landing.
//!
//! Bypasses RPC by sending transactions directly to leader validators
//! via QUIC. This reduces latency compared to Jito bundles when
//! frontrunning protection is not required.

use anyhow::{Context, Result};
use solana_client::connection_cache::ConnectionCache;
use solana_client::rpc_client::RpcClient;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;
use std::sync::Arc;
use tracing::{debug, info, warn};

const DEFAULT_FANOUT_SLOTS: u64 = 12;

/// Direct TPU sender that bypasses RPC for faster transaction landing.
///
/// Uses QUIC connections to send transactions directly to the next N
/// leaders in the validator schedule.
pub struct TpuSender {
    rpc_client: Arc<RpcClient>,
    websocket_url: String,
    fanout_slots: u64,
}

impl TpuSender {
    /// Create a new TPU sender.
    ///
    /// # Arguments
    /// * `rpc_url` - HTTP RPC endpoint URL
    /// * `ws_url` - WebSocket RPC endpoint URL for leader tracking
    /// * `fanout_slots` - Number of upcoming leader slots to send to (default: 12)
    pub fn new(rpc_url: &str, ws_url: &str, fanout_slots: Option<u64>) -> Self {
        let fanout = fanout_slots.unwrap_or(DEFAULT_FANOUT_SLOTS);

        info!(
            "Initializing TPU sender with fanout_slots={}, rpc={}",
            fanout, rpc_url
        );

        Self {
            rpc_client: Arc::new(RpcClient::new(rpc_url.to_string())),
            websocket_url: ws_url.to_string(),
            fanout_slots: fanout,
        }
    }

    /// Send a transaction directly to TPU leaders.
    ///
    /// This method sends the transaction to the next `fanout_slots` leaders
    /// for best chance of landing. Returns immediately after sending - does
    /// not wait for confirmation.
    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<Signature> {
        let signature = tx
            .signatures
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Transaction has no signatures"))?;

        let tx_clone = tx.clone();
        let rpc_client = self.rpc_client.clone();
        let ws_url = self.websocket_url.clone();
        let fanout = self.fanout_slots;

        debug!(
            "Sending transaction via TPU, fanout={}, sig={}",
            fanout, signature
        );

        tokio::task::spawn_blocking(move || -> Result<()> {
            let serialized =
                bincode::serialize(&tx_clone).context("Failed to serialize transaction")?;

            let connection_cache = ConnectionCache::new_quic("tpu_sender", 4);
            let config = TpuClientConfig {
                fanout_slots: fanout,
            };

            match connection_cache {
                ConnectionCache::Quic(cache) => {
                    let tpu_client =
                        TpuClient::new_with_connection_cache(rpc_client, &ws_url, config, cache)
                            .context("Failed to create TPU client")?;

                    tpu_client
                        .try_send_wire_transaction(serialized)
                        .map_err(|e| anyhow::anyhow!("TPU send failed: {:?}", e))
                }
                ConnectionCache::Udp(_) => {
                    anyhow::bail!("UDP connection cache not supported, expected QUIC")
                }
            }
        })
        .await
        .context("TPU send task panicked")??;

        debug!("Transaction sent via TPU: {}", signature);
        Ok(signature)
    }

    /// Send a transaction with retry on failure.
    pub async fn send_with_retry(
        &self,
        tx: &VersionedTransaction,
        max_retries: u32,
    ) -> Result<Signature> {
        let mut last_error = None;

        for attempt in 0..max_retries {
            match self.send_transaction(tx).await {
                Ok(sig) => return Ok(sig),
                Err(e) => {
                    warn!("TPU send attempt {} failed: {}", attempt + 1, e);
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Max retries exceeded")))
    }

    /// Send multiple transactions in parallel.
    pub async fn send_batch(&self, txs: &[VersionedTransaction]) -> Vec<Result<Signature>> {
        let mut results = Vec::with_capacity(txs.len());
        for tx in txs {
            results.push(self.send_transaction(tx).await);
        }
        results
    }

    /// Get the current fanout slots configuration.
    pub fn fanout_slots(&self) -> u64 {
        self.fanout_slots
    }

    /// Get a reference to the underlying RPC client.
    pub fn rpc_client(&self) -> &Arc<RpcClient> {
        &self.rpc_client
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_fanout() {
        assert_eq!(DEFAULT_FANOUT_SLOTS, 12);
    }

    #[test]
    fn test_tpu_sender_creation() {
        let sender = TpuSender::new(
            "https://api.mainnet-beta.solana.com",
            "wss://api.mainnet-beta.solana.com",
            Some(8),
        );
        assert_eq!(sender.fanout_slots(), 8);
    }

    #[test]
    fn test_tpu_sender_default_fanout() {
        let sender = TpuSender::new(
            "https://api.mainnet-beta.solana.com",
            "wss://api.mainnet-beta.solana.com",
            None,
        );
        assert_eq!(sender.fanout_slots(), DEFAULT_FANOUT_SLOTS);
    }
}
