use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const MAGIC_BYTES: u32 = 0xCAFEBABE;
pub const MAX_PACKET_SIZE: usize = 65535; // UDP Max (Safe ~1200, but buffer should be large)

/// The Domain Signal: What does the Overmind want?
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OvermindSignal {
    Ping,
    Buy { token: String, amount: f64 },
    SellAll { token: Option<String> }, // Fixed: Option<String> allows "Sell Specific" or "Sell Everything"
    Panic,                             // Emergency Shutdown (Drop everything, sell all)
    Sweep,                             // Consolidate funds
}

/// The Transport Envelope: Secure delivery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub magic: u32,
    pub version: u8,
    pub seq: u64,     // Monotonic sequence number for deduplication
    pub ts_ns: u64,   // Timestamp in nanoseconds for replay protection
    pub nonce: u64,   // XOR noise
    pub from: String, // Sender ID (e.g., "MASTER", "TOKYO-1")
    pub signal: OvermindSignal,
    pub signature: [u8; 32], // HMAC-SHA256
}

impl Envelope {
    /// Create and sign a new envelope
    pub fn new(
        seq: u64,
        ts_ns: u64,
        nonce: u64,
        from: String,
        signal: OvermindSignal,
        key: &[u8],
    ) -> Result<Self> {
        let mut envelope = Self {
            magic: MAGIC_BYTES,
            version: 1,
            seq,
            ts_ns,
            nonce,
            from,
            signal,
            signature: [0u8; 32],
        };
        envelope.sign(key)?;
        Ok(envelope)
    }

    /// Sign the envelope using HMAC-SHA256 over Bincode serialization of self (with sig=0)
    pub fn sign(&mut self, key: &[u8]) -> Result<()> {
        self.signature = [0u8; 32]; // Reset signature for canonical form
        let mut mac = HmacSha256::new_from_slice(key).context("HMAC key initialization failed")?;

        let payload = bincode::serialize(self).context("Envelope serialization failed")?;

        mac.update(&payload);
        self.signature = mac.finalize().into_bytes().into();
        Ok(())
    }

    /// Verify the envelope's signature, magic, and basic integrity
    pub fn verify(&self, key: &[u8]) -> Result<()> {
        if self.magic != MAGIC_BYTES {
            bail!("Invalid Magic Bytes: {:x}", self.magic);
        }
        if self.version != 1 {
            bail!("Unsupported Version: {}", self.version);
        }

        // 1. Reconstruct canonical form for verification
        let mut canonical = self.clone();
        canonical.signature = [0u8; 32];

        let mut mac = HmacSha256::new_from_slice(key).context("HMAC key invalid")?;

        let payload = bincode::serialize(&canonical).context("Canonical serialization failed")?;

        mac.update(&payload);

        // 2. Verify HMAC
        mac.verify_slice(&self.signature)
            .map_err(|_| anyhow::anyhow!("⛔ INVALID SIGNATURE - POTENTIAL SPOOFING"))?;

        Ok(())
    }

    /// Check if the timestamp is within the validity window (e.g., 5 seconds)
    pub fn is_fresh(&self, current_ts_ns: u64, max_drift_ns: u64) -> bool {
        if self.ts_ns > current_ts_ns {
            // Future timestamp? Allow small drift (e.g. 1 sec)
            return self.ts_ns - current_ts_ns < 1_000_000_000;
        }
        current_ts_ns - self.ts_ns < max_drift_ns
    }
}
