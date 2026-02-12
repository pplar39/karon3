//! Telegram notification service for trade alerts.
//! 
//! Listens to StreamEvent broadcast channel and sends alerts.
//! Uses teloxide with throttle to respect Telegram rate limits.

use crate::types::{StreamEvent, TradeAction};
use tokio::sync::broadcast;
use tracing::{info, warn};

pub struct NotificationService {
    bot_token: String,
    chat_id: i64,
    enabled: bool,
}

impl NotificationService {
    pub fn new(bot_token: Option<String>, chat_id: Option<i64>) -> Self {
        let enabled = bot_token.is_some() && chat_id.is_some();
        
        Self {
            bot_token: bot_token.unwrap_or_default(),
            chat_id: chat_id.unwrap_or(0),
            enabled,
        }
    }
    
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    
    /// Start listening to events and sending notifications
    pub async fn start(&self, mut rx: broadcast::Receiver<StreamEvent>) {
        if !self.enabled {
            info!("📵 Notifications disabled (no token/chat_id configured)");
            return;
        }
        
        use teloxide::prelude::*;
        use teloxide::adaptors::Throttle;
        
        let bot: Throttle<Bot> = Bot::new(&self.bot_token).throttle(Default::default());
        let chat_id = teloxide::types::ChatId(self.chat_id);
        
        info!("📱 Telegram notifications enabled for chat_id: {}", self.chat_id);
        
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(msg) = self.format_event(&event) {
                        let bot = bot.clone();
                        let chat = chat_id;
                        
                        // Fire and forget - don't block trading loop
                        tokio::spawn(async move {
                            if let Err(e) = bot.send_message(chat, &msg)
                                .parse_mode(teloxide::types::ParseMode::Html)
                                .await 
                            {
                                warn!("Failed to send Telegram alert: {}", e);
                            }
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Notification service lagged by {} events", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Broadcast channel closed, stopping notifications");
                    break;
                }
            }
        }
    }
    
    /// Send a direct message (for test-alert or critical alerts)
    pub async fn send_direct(&self, message: &str) -> anyhow::Result<()> {
        if !self.enabled {
            anyhow::bail!("Notifications not configured");
        }
        
        use teloxide::prelude::*;
        
        let bot = Bot::new(&self.bot_token);
        let chat_id = teloxide::types::ChatId(self.chat_id);
        
        bot.send_message(chat_id, message)
            .parse_mode(teloxide::types::ParseMode::Html)
            .await?;
        
        Ok(())
    }
    
    fn format_event(&self, event: &StreamEvent) -> Option<String> {
        match event {
            StreamEvent::TradeExecuted(trade) => {
                let emoji = match trade.action {
                    TradeAction::Buy => "🟢",
                    TradeAction::Sell => "🔴",
                };
                let pnl_str = if trade.pnl_percent != 0.0 {
                    format!("\nPnL: <b>{:+.2}%</b> ({:+.4} SOL)", trade.pnl_percent, trade.pnl_sol)
                } else {
                    String::new()
                };
                
                Some(format!(
                    "{} <b>{:?}</b>\nAmount: {:.4} SOL\nPrice: {:.6} SOL{}",
                    emoji, trade.action, trade.amount_sol, trade.price_sol, pnl_str
                ))
            }
            StreamEvent::TradeFailed { reason, pool_id } => {
                let pool_str = pool_id.map(|p| format!("\nPool: {}", p)).unwrap_or_default();
                Some(format!("⚠️ <b>Trade Failed</b>\n{}{}", reason, pool_str))
            }
            StreamEvent::CircuitBreakerTriggered { reason } => {
                Some(format!("🛑 <b>CIRCUIT BREAKER</b>\n{}", reason))
            }
            StreamEvent::DailyLossLimitReached { loss_sol, limit_sol } => {
                Some(format!(
                    "🚨 <b>DAILY LOSS LIMIT REACHED</b>\nLoss: {:.4} SOL / Limit: {:.4} SOL\n\n<i>Trading halted until tomorrow (UTC)</i>",
                    loss_sol, limit_sol
                ))
            }
            StreamEvent::Error(msg) => {
                Some(format!("⚠️ Error: {}", msg))
            }
            // Don't notify for every pool/price update
            StreamEvent::NewPool(_, _) | StreamEvent::PriceUpdate { .. } => None,
        }
    }
}
