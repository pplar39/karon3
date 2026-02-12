use crate::types::{
    ExitReason, MarketContext, OrderIntent, OrderKind, Pool, Position, Side, TradingError,
};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct OmegaTrinityConfig {
    pub momentum_threshold: Decimal,
    pub entry_max_spread: u32,
    pub depth_imbalance_buy_threshold: Decimal,
    pub depth_imbalance_sell_threshold: Decimal,
    pub default_qty: Decimal,
}

impl Default for OmegaTrinityConfig {
    fn default() -> Self {
        Self {
            momentum_threshold: Decimal::new(1, 3), // 0.001
            entry_max_spread: 10,
            depth_imbalance_buy_threshold: Decimal::new(12, 1), // 1.2
            depth_imbalance_sell_threshold: Decimal::new(8, 1), // 0.8
            default_qty: Decimal::ONE,
        }
    }
}

pub struct OmegaTrinityEngine {
    config: OmegaTrinityConfig,
}

impl OmegaTrinityEngine {
    pub fn new(config: OmegaTrinityConfig) -> Self {
        Self { config }
    }

    /// 3-Rule Alpha Engine:
    /// 1) Momentum Ignition
    /// 2) Spread Feasibility  
    /// 3) Depth Imbalance
    pub fn generate_intent(
        &self,
        ctx: &MarketContext,
        pos_is_empty: bool,
        pos_side: Option<Side>,
        pos_qty: Decimal,
    ) -> Result<Option<(OrderIntent, Side, OrderKind, Decimal)>, TradingError> {
        let tick_delta = ctx.tick.0.saturating_sub(ctx.last_tick.0);
        if tick_delta == 0 {
            return Ok(None);
        }

        let price_delta = ctx.mid.0 - ctx.last_mid.0;
        let velocity = price_delta / Decimal::from(tick_delta);

        let momentum_signal = if velocity > self.config.momentum_threshold {
            Some(Side::Buy)
        } else if velocity < -self.config.momentum_threshold {
            Some(Side::Sell)
        } else {
            None
        };

        let spread_ok = ctx.spread_ticks <= self.config.entry_max_spread;

        let depth_signal = if ctx.depth_imbalance >= self.config.depth_imbalance_buy_threshold {
            Some(Side::Buy)
        } else if ctx.depth_imbalance <= self.config.depth_imbalance_sell_threshold {
            Some(Side::Sell)
        } else {
            None
        };

        // Exit first
        if !pos_is_empty {
            let side = pos_side
                .ok_or_else(|| TradingError::AlphaReject("Position side missing".into()))?;
            let should_exit = match side {
                Side::Buy => velocity < -self.config.momentum_threshold,
                Side::Sell => velocity > self.config.momentum_threshold,
            };

            if should_exit {
                let exit_side = match side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                };
                return Ok(Some((
                    OrderIntent::Exit {
                        reason: ExitReason::StopLoss,
                    },
                    exit_side,
                    OrderKind::Market,
                    pos_qty,
                )));
            }
        }

        // Entry
        if pos_is_empty && spread_ok {
            match (momentum_signal, depth_signal) {
                (Some(m_side), Some(d_side)) if m_side == d_side => {
                    return Ok(Some((
                        OrderIntent::Entry,
                        m_side,
                        OrderKind::Market,
                        self.config.default_qty,
                    )));
                }
                _ => {}
            }
        }

        Ok(None)
    }
}
