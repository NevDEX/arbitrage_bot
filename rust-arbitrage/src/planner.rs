//! Replace `Strategy` to plug a different signal into the same execution engine.
use crate::{
    config::Config,
    market::{Depth, Market},
    numbers::*,
};
use anyhow::Result;
use num_bigint::BigInt;
use num_traits::Zero;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}
impl Side {
    pub fn opposite(self) -> Self {
        if self == Self::Buy {
            Self::Sell
        } else {
            Self::Buy
        }
    }
    pub fn is_buy(self) -> bool {
        self == Self::Buy
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Quote {
    pub side: Side,
    pub price: String,
    pub amount: String,
}
#[derive(Clone, Debug)]
pub struct Opportunity {
    pub route: &'static str,
    pub quote: Quote,
    pub cost: BigInt,
    pub proceeds: BigInt,
    pub net: BigInt,
    pub commitment: BigInt,
    pub created_at: u64,
}
pub struct StrategyInput<'a> {
    pub config: &'a Config,
    pub market: &'a Market,
    pub depth: &'a Depth,
    pub uni_sell: &'a BigInt,
    pub uni_buy: &'a BigInt,
    pub created_at: u64,
}
pub trait Strategy: Send + Sync {
    fn evaluate(&self, input: StrategyInput<'_>) -> Result<Option<Opportunity>>;
}
#[derive(Default)]
pub struct CrossVenue;
impl Strategy for CrossVenue {
    fn evaluate(&self, i: StrategyInput<'_>) -> Result<Option<Opportunity>> {
        let (c, m) = (i.config, i.market);
        i.depth.validate()?;
        let base = units(&c.order_amount, m.base_token_decimals)?;
        let gas = units(&c.gas_cost_quote, m.quote_token_decimals)?;
        let minimum = units(&c.min_profit_quote, m.quote_token_decimals)?;
        let cap = units(&c.max_quote_per_trade, m.quote_token_decimals)?;
        let mut best: Option<Opportunity> = None;
        for side in [Side::Sell, Side::Buy] {
            let Some(price) = i.depth.limit(side == Side::Sell, &base, m)? else {
                continue;
            };
            if decimal(&price)? * decimal(&c.order_amount)? < decimal(&m.min_order_size)? {
                continue;
            }
            let gross = notional(&price, &c.order_amount, m.quote_token_decimals)?;
            if gross <= BigInt::zero() {
                continue;
            }
            let order_cap = &gross + m.fee(&gross)?;
            let mut cost = if side == Side::Sell {
                slip(i.uni_buy, c.slippage_bps, true)
            } else {
                order_cap.clone()
            };
            let proceeds = if side == Side::Sell {
                &gross - m.fee(&gross)?
            } else {
                slip(i.uni_sell, c.slippage_bps, false)
            };
            cost += if side == Side::Sell {
                i.depth.bids.len()
            } else {
                i.depth.asks.len()
            };
            let net = &proceeds - &cost - &gas;
            let commitment = order_cap.max(cost.clone());
            if commitment > cap
                || proceeds > cap
                || net < minimum
                || !ratio_pass(&net, &cost, &c.min_profit_pct)?
            {
                continue;
            }
            if best.as_ref().is_none_or(|b| net > b.net) {
                best = Some(Opportunity {
                    route: if side == Side::Sell {
                        "BUY_UNI_SELL_MYOB"
                    } else {
                        "BUY_MYOB_SELL_UNI"
                    },
                    quote: Quote {
                        side,
                        price,
                        amount: c.order_amount.clone(),
                    },
                    cost,
                    proceeds,
                    net,
                    commitment,
                    created_at: i.created_at,
                });
            }
        }
        Ok(best)
    }
}
