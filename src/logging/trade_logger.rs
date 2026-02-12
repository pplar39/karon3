use crate::types::{Metrics, TradeResult};
use anyhow::Result;
use csv::Writer;
use std::fs::{File, OpenOptions};
use std::path::Path;
use tracing::info;

pub struct TradeLogger {
    trades_path: String,
    metrics: Metrics,
}

impl TradeLogger {
    pub fn new(trades_path: &str) -> Result<Self> {
        if !Path::new(trades_path).exists() {
            let file = File::create(trades_path)?;
            let mut wtr = Writer::from_writer(file);
            wtr.write_record(&[
                "timestamp",
                "position_id",
                "action",
                "price_sol",
                "amount_sol",
                "pnl_percent",
                "pnl_sol",
                "reason",
            ])?;
            wtr.flush()?;
        }

        Ok(Self {
            trades_path: trades_path.to_string(),
            metrics: Metrics::default(),
        })
    }

    pub fn log_trade(&mut self, trade: &TradeResult) -> Result<()> {
        let file = OpenOptions::new().append(true).open(&self.trades_path)?;

        let mut wtr = Writer::from_writer(file);

        wtr.write_record(&[
            trade.timestamp.to_rfc3339(),
            trade.position_id.clone(),
            format!("{:?}", trade.action),
            trade.price_sol.to_string(),
            trade.amount_sol.to_string(),
            trade.pnl_percent.to_string(),
            trade.pnl_sol.to_string(),
            trade.reason.clone(),
        ])?;

        wtr.flush()?;

        self.update_metrics(trade);

        info!(
            "📝 Logged trade: {} {:?} at {} SOL",
            trade.position_id, trade.action, trade.price_sol
        );

        Ok(())
    }

    fn update_metrics(&mut self, trade: &TradeResult) {
        self.metrics.total_trades += 1;

        if trade.pnl_percent > 0.0 {
            self.metrics.winning_trades += 1;
        } else if trade.pnl_percent < 0.0 {
            self.metrics.losing_trades += 1;
        }

        self.metrics.total_pnl_sol += trade.pnl_sol;

        if trade.pnl_sol > self.metrics.best_trade_pnl {
            self.metrics.best_trade_pnl = trade.pnl_sol;
        }
        if trade.pnl_sol < self.metrics.worst_trade_pnl {
            self.metrics.worst_trade_pnl = trade.pnl_sol;
        }
    }

    pub fn get_metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn set_wallet_balance(&mut self, balance_sol: f64) {
        self.metrics.wallet_balance_sol = balance_sol;
    }

    pub fn calculate_daily_pnl(&self, day: chrono::NaiveDate) -> Result<f64> {
        if !Path::new(&self.trades_path).exists() {
            return Ok(0.0);
        }

        let mut rdr = csv::Reader::from_path(&self.trades_path)?;
        let mut daily_pnl = 0.0_f64;

        for row in rdr.records() {
            let rec = match row {
                Ok(v) => v,
                Err(_) => continue,
            };
            if rec.len() < 7 {
                continue;
            }

            let ts = match chrono::DateTime::parse_from_rfc3339(&rec[0]) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if ts.date_naive() != day {
                continue;
            }

            if let Ok(pnl) = rec[6].parse::<f64>() {
                daily_pnl += pnl;
            }
        }

        Ok(daily_pnl)
    }

    pub fn print_summary(&self) {
        let m = &self.metrics;
        let win_rate = if m.total_trades > 0 {
            (m.winning_trades as f64 / m.total_trades as f64) * 100.0
        } else {
            0.0
        };

        info!("📊 TRADING SUMMARY");
        info!("   Total Trades: {}", m.total_trades);
        info!("   Win Rate: {:.1}%", win_rate);
        info!("   Total PnL: {:.4} SOL", m.total_pnl_sol);
        info!("   Best Trade: {:.4} SOL", m.best_trade_pnl);
        info!("   Worst Trade: {:.4} SOL", m.worst_trade_pnl);
    }
}
