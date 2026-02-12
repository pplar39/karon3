use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitoDailyReport {
    pub date: String,
    pub total_bundles_sent: usize,
    pub total_bundles_landed: usize,
    pub total_tips_paid_lamports: u64,
    pub average_latency_ms: u64,
    pub p99_latency_ms: u64,
}

pub struct JitoMetrics {
    bundles_sent: AtomicUsize,
    bundles_landed: AtomicUsize,
    tips_paid: AtomicU64,
    latencies: Arc<RwLock<Vec<u64>>>,
    report_dir: PathBuf,
}

impl JitoMetrics {
    pub fn new(report_dir: PathBuf) -> Self {
        Self {
            bundles_sent: AtomicUsize::new(0),
            bundles_landed: AtomicUsize::new(0),
            tips_paid: AtomicU64::new(0),
            latencies: Arc::new(RwLock::new(Vec::new())),
            report_dir,
        }
    }

    pub fn record_sent(&self) {
        self.bundles_sent.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_landed(&self, tip_lamports: u64, latency_ms: u64) {
        self.bundles_landed.fetch_add(1, Ordering::SeqCst);
        self.tips_paid.fetch_add(tip_lamports, Ordering::SeqCst);
        
        let latencies = self.latencies.clone();
        tokio::spawn(async move {
            let mut lock = latencies.write().await;
            lock.push(latency_ms);
        });
    }

    pub async fn generate_daily_report(&self) -> anyhow::Result<()> {
        let latencies = self.latencies.read().await;
        let avg_latency = if latencies.is_empty() {
            0
        } else {
            latencies.iter().sum::<u64>() / latencies.len() as u64
        };

        let mut sorted_latencies = latencies.clone();
        sorted_latencies.sort();
        let p99_index = (sorted_latencies.len() as f64 * 0.99) as usize;
        let p99_latency = if sorted_latencies.is_empty() {
            0
        } else {
            sorted_latencies[p99_index.min(sorted_latencies.len().saturating_sub(1))]
        };

        let report = JitoDailyReport {
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            total_bundles_sent: self.bundles_sent.load(Ordering::SeqCst),
            total_bundles_landed: self.bundles_landed.load(Ordering::SeqCst),
            total_tips_paid_lamports: self.tips_paid.load(Ordering::SeqCst),
            average_latency_ms: avg_latency,
            p99_latency_ms: p99_latency,
        };

        let json = serde_json::to_string_pretty(&report)?;
        let filename = format!("{}_report.json", report.date);
        let path = self.report_dir.join(filename);

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        file.write_all(json.as_bytes())?;

        Ok(())
    }
}
