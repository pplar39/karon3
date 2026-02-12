use crate::trading::hot_blockhash::HotBlockhashMonitor;
use crate::trading::jito::JitoSender;
use crate::trading::overmind_protocol::{Envelope, OvermindSignal, MAX_PACKET_SIZE};
use crate::trading::pump_fun_swap::PumpFunSwapBuilder;
use crate::trading::{load_keypair_from_env, load_keypair_from_file};
use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub struct OvermindEdge {
    socket: UdpSocket,
    region: String,
    master_udp: SocketAddr,
    master_pubkey: Pubkey,
    hmac_key: [u8; 32],
    keypair: Arc<Keypair>,
    hot_blockhash: Arc<HotBlockhashMonitor>,
    rpc_client: Arc<RpcClient>,
    jito_sender: Option<Arc<JitoSender>>,

    // Dedup state
    last_seq: std::sync::atomic::AtomicU64,
}

impl OvermindEdge {
    pub async fn new(
        bind_addr: &str,
        master_udp: &str,
        master_pubkey_str: &str,
        region: &str,
        hmac_key_str: &str,
        hot_blockhash: Arc<HotBlockhashMonitor>,
        rpc_client: Arc<RpcClient>,
        jito_sender: Option<Arc<JitoSender>>,
    ) -> Result<Self> {
        // 1. Async Bind
        let socket = UdpSocket::bind(bind_addr)
            .await
            .context("Failed to bind Edge UDP socket")?;

        info!("[OVERMIND-EDGE:{}] Online at {}", region, bind_addr);

        // 2. Address & Auth Setup
        let master_udp: SocketAddr = master_udp
            .parse()
            .context("Invalid Master UDP Address (IP:Port required)")?;

        let master_pubkey = Pubkey::from_str(master_pubkey_str).context("Invalid Master Pubkey")?;

        let mut hmac_key = [0u8; 32];
        let key_bytes = bs58::decode(hmac_key_str)
            .into_vec()
            .context("Invalid HMAC Key (Base58 required)")?;
        if key_bytes.len() != 32 {
            anyhow::bail!("HMAC Key must be 32 bytes");
        }
        hmac_key.copy_from_slice(&key_bytes);

        // 3. Wallet Loading
        let wallet_filename = format!("id_{}.json", region.to_lowercase());
        let keypair = if std::path::Path::new(&wallet_filename).exists() {
            info!(
                "[OVERMIND-EDGE:{}] Loading Independent Wallet: {}",
                region, wallet_filename
            );
            load_keypair_from_file(&wallet_filename)?
        } else {
            warn!(
                "[OVERMIND-EDGE:{}] Region wallet {} not found, using fallback env",
                region, wallet_filename
            );
            load_keypair_from_env("WALLET_KEYPAIR").context("No fallback wallet found")?
        };

        Ok(Self {
            socket,
            region: region.to_string(),
            master_udp,
            master_pubkey,
            hmac_key,
            keypair: Arc::new(keypair),
            hot_blockhash,
            rpc_client,
            jito_sender,
            last_seq: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Main Event Loop - "The Listener"
    /// Runs forever, processing signals
    pub async fn run_listener(self: Arc<Self>) -> Result<()> {
        let mut buf = [0u8; MAX_PACKET_SIZE];

        info!(
            "[OVERMIND-EDGE:{}] Listening for Command & Control...",
            self.region
        );

        loop {
            // Async Recv
            let (amt, src) = match self.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    error!("[EDGE] Recv failed: {}", e);
                    continue; // Keep listening
                }
            };

            // 1. IP Allowlist Check (Basic Spoofing Protection)
            // Ideally we check if src == self.master_udp, but master IP might float?
            // For strict mode:
            if src != self.master_udp {
                warn!("[EDGE] Dropped packet from unknown source: {}", src);
                continue;
            }

            // 2. Deserialization
            let envelope: Envelope = match bincode::deserialize(&buf[..amt]) {
                Ok(env) => env,
                Err(e) => {
                    warn!("[EDGE] Malformed packet ({} bytes): {}", amt, e);
                    continue;
                }
            };

            // 3. Verification (HMAC + Replay)
            if let Err(e) = envelope.verify(&self.hmac_key) {
                error!("[EDGE] AUTH FAILURE: {}", e);
                continue;
            }

            // 4. Time & Seq Check
            let now_ns = crate::time::monotonic_now_ns();

            if !envelope.is_fresh(now_ns, 5_000_000_000) {
                // 5s window
                warn!(
                    "[EDGE] Stale packet dropped (Delta: {}ms)",
                    (now_ns as i128 - envelope.ts_ns as i128) / 1_000_000
                );
                continue;
            }

            let last = self.last_seq.load(std::sync::atomic::Ordering::Relaxed);
            if envelope.seq <= last {
                // Duplicate or Out-of-order
                debug!("[EDGE] Duplicate Seq {} <= Last {}", envelope.seq, last);
                continue;
            }
            self.last_seq
                .store(envelope.seq, std::sync::atomic::Ordering::Relaxed);

            // 5. Dispatch
            let self_clone = self.clone();
            let signal = envelope.signal.clone();

            // Spawn execution to not block listener
            tokio::spawn(async move {
                if let Err(e) = self_clone.execute_reflex(signal).await {
                    error!("[EDGE] Reflex Error: {}", e);
                }
            });
        }
    }

    pub async fn execute_reflex(&self, signal: OvermindSignal) -> Result<()> {
        match signal {
            OvermindSignal::Ping => {
                info!("[OVERMIND] Ping received. Pong.");
                // TODO: Send Ack back
            }
            OvermindSignal::Buy { token, amount } => {
                info!(
                    "[CERBERUS:{}] EXECUTING ORBITAL STRIKE BUY: {} SOL: {}",
                    self.region, token, amount
                );
                self.execute_orbital_strike_buy(&token, amount).await?;
            }
            OvermindSignal::SellAll { token } => {
                let target = token.as_deref().unwrap_or("ALL");
                info!(
                    "[CERBERUS:{}] ACTIVATING IRON DOME (PANIC SELL): {}",
                    self.region, target
                );
                self.execute_sell_all(token).await?;
            }
            OvermindSignal::Panic => {
                error!(
                    "[CERBERUS:{}] !!! SYSTEM PANIC TRIGGERED !!! DUMPING EVERYTHING",
                    self.region
                );
                self.execute_sell_all(None).await?;
            }
            OvermindSignal::Sweep => {
                info!(
                    "[CERBERUS:{}] INIT OPERATION VACUUM (SWEEPING FUNDS)",
                    self.region
                );
                self.execute_sweep().await?;
            }
        }
        Ok(())
    }

    async fn execute_orbital_strike_buy(&self, token_mint: &str, sol_amount: f64) -> Result<()> {
        let sol_lamports = (sol_amount * 1_000_000_000.0) as u64;
        let _token_amount = PumpFunSwapBuilder::calculate_tokens_for_sol(sol_lamports);

        // Placeholder for actual swap instruction builder
        // For MVP rescue, we build a dummy transfer or Memo to verify pipeline

        let recent_blockhash = self.hot_blockhash.get_hot_hash().await;

        let mut tx = Transaction::new_with_payer(&[], Some(&self.keypair.pubkey()));
        tx.sign(&[&*self.keypair], recent_blockhash);

        self.send_tx(&tx).await
    }

    async fn execute_sweep(&self) -> Result<()> {
        let balance = self.rpc_client.get_balance(&self.keypair.pubkey()).await?;
        let reserve = 20_000_000; // 0.02 SOL

        if balance <= reserve {
            return Ok(());
        }

        let sweep_amount = balance - reserve;
        info!(
            "[VACUUM] Sweeping {} lamports to Master ({})",
            sweep_amount, self.master_pubkey
        );

        let instruction = solana_sdk::system_instruction::transfer(
            &self.keypair.pubkey(),
            &self.master_pubkey,
            sweep_amount,
        );

        let recent_blockhash = self.hot_blockhash.get_hot_hash().await;
        let mut tx = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&self.keypair.pubkey()),
            &[&*self.keypair],
            recent_blockhash,
        );

        self.send_tx(&tx).await
    }

    async fn execute_sell_all(&self, _token: Option<String>) -> Result<()> {
        // MVP: Just send a signal tx for now until PositionManager is integrated
        info!("[IRON-DOME] Scanning for assets to liquidate...");

        let recent_blockhash = self.hot_blockhash.get_hot_hash().await;
        let mut tx = Transaction::new_with_payer(&[], Some(&self.keypair.pubkey()));
        tx.sign(&[&*self.keypair], recent_blockhash);

        self.send_tx(&tx).await
    }

    // Unified TX Sender (Jito > RPC)
    async fn send_tx(&self, tx: &Transaction) -> Result<()> {
        if let Some(jito) = &self.jito_sender {
            let bundle: Vec<VersionedTransaction> = vec![tx.clone().into()];
            match jito.send_bundle(bundle).await {
                Ok(uuid) => {
                    info!("[JITO] Bundle Sent: {}", uuid);
                    return Ok(());
                }
                Err(e) => error!("[JITO] Failed: {}", e),
            }
        }

        // Fallback
        match self.rpc_client.send_transaction(tx).await {
            Ok(sig) => info!("[RPC] Sent: {}", sig),
            Err(e) => error!("[RPC] Failed: {}", e),
        }
        Ok(())
    }
}
