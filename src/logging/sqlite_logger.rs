//! SQLite-based trade logger for persistent trade history and analytics.

use crate::types::{TradeResult, TradeAction};
use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, params, Row, Result as SqlResult};
use std::sync::Mutex;
use tracing::{info, debug};

/// Bundle status for landing rate tracking
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BundleStatus {
    Pending,
    Landed,
    Failed,
    Expired,
}

impl BundleStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BundleStatus::Pending => "pending",
            BundleStatus::Landed => "landed",
            BundleStatus::Failed => "failed",
            BundleStatus::Expired => "expired",
        }
    }
    
    pub fn from_str(s: &str) -> Self {
        match s {
            "landed" => BundleStatus::Landed,
            "failed" => BundleStatus::Failed,
            "expired" => BundleStatus::Expired,
            _ => BundleStatus::Pending,
        }
    }
}

/// SQLite-based trade and bundle logger
pub struct SqliteLogger {
    conn: Mutex<Connection>,
}

impl SqliteLogger {
    /// Create a new SQLite logger
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path)
            .context("Failed to open SQLite database")?;
        
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                position_id TEXT NOT NULL,
                action TEXT NOT NULL,
                price_sol REAL NOT NULL,
                amount_sol REAL NOT NULL,
                pnl_percent REAL NOT NULL,
                pnl_sol REAL NOT NULL,
                reason TEXT,
                bundle_id TEXT,
                is_shadow INTEGER DEFAULT 0,
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            
            CREATE TABLE IF NOT EXISTS bundles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bundle_id TEXT UNIQUE NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                submitted_at TEXT NOT NULL,
                landed_at TEXT,
                slot INTEGER,
                tip_lamports INTEGER,
                tx_count INTEGER DEFAULT 1,
                error_message TEXT,
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            
            CREATE INDEX IF NOT EXISTS idx_trades_timestamp ON trades(timestamp);
            CREATE INDEX IF NOT EXISTS idx_trades_position ON trades(position_id);
            CREATE INDEX IF NOT EXISTS idx_bundles_status ON bundles(status);
            CREATE INDEX IF NOT EXISTS idx_bundles_submitted ON bundles(submitted_at);"
        ).context("Failed to create tables")?;
        
        info!("SQLite logger initialized: {}", db_path);
        
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    
    /// Log a trade to the database
    pub fn log_trade(&self, trade: &TradeResult, bundle_id: Option<&str>, is_shadow: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        conn.execute(
            "INSERT INTO trades 
             (timestamp, position_id, action, price_sol, amount_sol, pnl_percent, pnl_sol, reason, bundle_id, is_shadow)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                trade.timestamp.to_rfc3339(),
                trade.position_id,
                format!("{:?}", trade.action),
                trade.price_sol,
                trade.amount_sol,
                trade.pnl_percent,
                trade.pnl_sol,
                trade.reason,
                bundle_id,
                is_shadow as i32,
            ],
        ).context("Failed to insert trade")?;
        
        debug!("Logged trade: {} {:?} (shadow: {})", trade.position_id, trade.action, is_shadow);
        
        Ok(())
    }
    
    /// Record a bundle submission
    pub fn record_bundle_submitted(&self, bundle_id: &str, tip_lamports: u64, tx_count: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        conn.execute(
            "INSERT OR REPLACE INTO bundles 
             (bundle_id, status, submitted_at, tip_lamports, tx_count)
             VALUES (?1, 'pending', ?2, ?3, ?4)",
            params![
                bundle_id,
                Utc::now().to_rfc3339(),
                tip_lamports as i64,
                tx_count as i32,
            ],
        ).context("Failed to record bundle")?;
        
        debug!("Recorded bundle submission: {}", bundle_id);
        
        Ok(())
    }
    
    /// Update bundle status
    pub fn update_bundle_status(&self, bundle_id: &str, status: BundleStatus, slot: Option<u64>, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        let landed_at = if status == BundleStatus::Landed {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        
        conn.execute(
            "UPDATE bundles 
             SET status = ?1, landed_at = ?2, slot = ?3, error_message = ?4
             WHERE bundle_id = ?5",
            params![
                status.as_str(),
                landed_at,
                slot.map(|s| s as i64),
                error,
                bundle_id,
            ],
        ).context("Failed to update bundle status")?;
        
        debug!("Updated bundle {} status to {:?}", bundle_id, status);
        
        Ok(())
    }
    
    /// Calculate daily PnL
    pub fn calculate_daily_pnl(&self, date: NaiveDate) -> Result<f64> {
        let conn = self.conn.lock().unwrap();
        let date_str = date.format("%Y-%m-%d").to_string();
        
        let pnl: f64 = conn.query_row(
            "SELECT COALESCE(SUM(pnl_sol), 0.0) 
             FROM trades 
             WHERE DATE(timestamp) = ?1 AND is_shadow = 0",
            params![date_str],
            |row: &Row| -> SqlResult<f64> { row.get(0) },
        ).unwrap_or(0.0);
        
        Ok(pnl)
    }
    
    /// Get landing rate statistics
    pub fn get_landing_rate(&self, hours: u64) -> Result<LandingRateStats> {
        let conn = self.conn.lock().unwrap();
        
        let cutoff = Utc::now() - chrono::Duration::hours(hours as i64);
        let cutoff_str = cutoff.to_rfc3339();
        
        let (total, landed, failed, pending): (i64, i64, i64, i64) = conn.query_row(
            "SELECT 
                COUNT(*) as total,
                SUM(CASE WHEN status = 'landed' THEN 1 ELSE 0 END) as landed,
                SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) as failed,
                SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END) as pending
             FROM bundles 
             WHERE submitted_at >= ?1",
            params![cutoff_str],
            |row: &Row| -> SqlResult<(i64, i64, i64, i64)> { Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)) },
        ).unwrap_or((0, 0, 0, 0));
        
        let rate = if total > 0 {
            (landed as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        
        Ok(LandingRateStats {
            total_bundles: total as u64,
            landed: landed as u64,
            failed: failed as u64,
            pending: pending as u64,
            landing_rate_percent: rate,
            window_hours: hours,
        })
    }
    
    /// Get trade statistics
    pub fn get_trade_stats(&self, days: u64) -> Result<TradeStats> {
        let conn = self.conn.lock().unwrap();
        
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();
        
        let stats: (i64, i64, i64, f64, f64, f64) = conn.query_row(
            "SELECT 
                COUNT(*) as total,
                SUM(CASE WHEN pnl_sol > 0 THEN 1 ELSE 0 END) as wins,
                SUM(CASE WHEN pnl_sol < 0 THEN 1 ELSE 0 END) as losses,
                COALESCE(SUM(pnl_sol), 0.0) as total_pnl,
                COALESCE(MAX(pnl_sol), 0.0) as best,
                COALESCE(MIN(pnl_sol), 0.0) as worst
             FROM trades 
             WHERE timestamp >= ?1 AND is_shadow = 0 AND action = 'Sell'",
            params![cutoff_str],
            |row: &Row| -> SqlResult<(i64, i64, i64, f64, f64, f64)> { Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)) },
        ).unwrap_or((0, 0, 0, 0.0, 0.0, 0.0));
        
        let win_rate = if stats.0 > 0 {
            (stats.1 as f64 / stats.0 as f64) * 100.0
        } else {
            0.0
        };
        
        Ok(TradeStats {
            total_trades: stats.0 as u64,
            winning_trades: stats.1 as u64,
            losing_trades: stats.2 as u64,
            total_pnl_sol: stats.3,
            best_trade_pnl: stats.4,
            worst_trade_pnl: stats.5,
            win_rate_percent: win_rate,
            window_days: days,
        })
    }
}

/// Landing rate statistics
#[derive(Debug, Clone)]
pub struct LandingRateStats {
    pub total_bundles: u64,
    pub landed: u64,
    pub failed: u64,
    pub pending: u64,
    pub landing_rate_percent: f64,
    pub window_hours: u64,
}

impl std::fmt::Display for LandingRateStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Landing Rate ({}h): {:.1}% ({}/{} landed, {} failed, {} pending)",
            self.window_hours,
            self.landing_rate_percent,
            self.landed,
            self.total_bundles,
            self.failed,
            self.pending
        )
    }
}

/// Trade statistics
#[derive(Debug, Clone)]
pub struct TradeStats {
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub total_pnl_sol: f64,
    pub best_trade_pnl: f64,
    pub worst_trade_pnl: f64,
    pub win_rate_percent: f64,
    pub window_days: u64,
}

impl std::fmt::Display for TradeStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Trades ({}d): {} total, {:.1}% win rate, {:.4} SOL PnL",
            self.window_days,
            self.total_trades,
            self.win_rate_percent,
            self.total_pnl_sol
        )
    }
}
