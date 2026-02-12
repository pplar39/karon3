use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct HotBlockhashMonitor {
    rpc_client: Arc<RpcClient>,
    latest_blockhash: Arc<RwLock<Hash>>,
}

impl HotBlockhashMonitor {
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self {
            rpc_client,
            latest_blockhash: Arc::new(RwLock::new(Hash::default())),
        }
    }

    pub async fn start(&self) {
        let rpc = self.rpc_client.clone();
        let cache = self.latest_blockhash.clone();

        info!("[HOT-BLOCKHASH] Starting atomic monitor spinlock...");

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(200)); // Poliing every 200ms
                                                                                  // In a real HFT setup, this might be a busy loop or closer to 50ms,
                                                                                  // but 200ms is safe for standard RPC limits.

            loop {
                interval.tick().await;
                // Get latest blockhash with commitment 'confirmed' or 'finalized'
                match rpc.get_latest_blockhash().await {
                    Ok(hash) => {
                        let mut w = cache.write().await;
                        if *w != hash {
                            *w = hash;
                            // info!("[HOT-BLOCKHASH] Updated: {}", hash); // Verbose log
                        }
                    }
                    Err(e) => {
                        error!("[HOT-BLOCKHASH] Fetch Failed: {}", e);
                    }
                }
            }
        });
    }

    pub async fn get_hot_hash(&self) -> Hash {
        *self.latest_blockhash.read().await
    }
}
