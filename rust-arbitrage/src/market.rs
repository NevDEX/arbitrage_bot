use crate::{config::Config, numbers::*};
use anyhow::{Result, ensure};
use num_bigint::BigInt;
use num_traits::Zero;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    pub id: String,
    pub is_published: bool,
    pub base_token_address: String,
    pub quote_token_address: String,
    pub base_token_decimals: u32,
    pub quote_token_decimals: u32,
    pub price_decimals: u32,
    pub amount_decimals: u32,
    #[serde(deserialize_with = "wire_string")]
    pub min_order_size: String,
    #[serde(deserialize_with = "wire_string")]
    pub maker_fee_rate: String,
    #[serde(deserialize_with = "wire_string")]
    pub taker_fee_rate: String,
}
impl Market {
    pub fn validate(&self, c: &Config) -> Result<()> {
        ensure!(
            self.id == c.market
                && self.is_published
                && same(&self.base_token_address, &c.base_token)
                && same(&self.quote_token_address, &c.quote_token),
            "Market deployment mismatch"
        );
        ensure!(
            [
                self.base_token_decimals,
                self.quote_token_decimals,
                self.price_decimals,
                self.amount_decimals
            ]
            .iter()
            .all(|d| *d <= 18)
                && self.amount_decimals <= self.base_token_decimals,
            "Invalid market precision"
        );
        decimal(&self.min_order_size)?;
        for rate in [&self.maker_fee_rate, &self.taker_fee_rate] {
            ensure!(
                units(rate, 5)? <= BigInt::from(65535),
                "Fee exceeds signed uint16"
            );
            ensure!(
                decimal(rate)? <= decimal(&c.max_fee_rate)?,
                "Market fee exceeds maxFeeRate"
            );
        }
        units(&c.order_amount, self.base_token_decimals)?;
        units(&c.order_amount, self.amount_decimals)?;
        for s in [
            &c.gas_cost_quote,
            &c.min_profit_quote,
            &c.max_quote_per_trade,
            &c.max_total_quote,
            &c.max_loss_quote,
        ] {
            units(s, self.quote_token_decimals)?;
        }
        units(&c.max_gas_price_gwei, 9)?;
        units(&c.max_hedge_gas_native, 18)?;
        Ok(())
    }
    pub fn fee(&self, amount: &BigInt) -> Result<BigInt> {
        let rate = units(&self.maker_fee_rate, 5)?.max(units(&self.taker_fee_rate, 5)?);
        Ok(ceil_div(&(amount * rate), &BigInt::from(100000)))
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct Depth {
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
    #[serde(deserialize_with = "wire_u64")]
    pub sequence: u64,
}
impl Depth {
    pub fn validate(&self) -> Result<()> {
        for (i, levels) in [&self.bids, &self.asks].iter().enumerate() {
            ensure!(levels.len() <= 10000, "Book exceeds level limit");
            let mut previous = None;
            for [p, a] in levels.iter() {
                positive(p)?;
                positive(a)?;
                let p = decimal(p)?;
                if let Some(prev) = previous {
                    ensure!(
                        if i == 0 { p < prev } else { p > prev },
                        "Unsorted order book"
                    );
                }
                previous = Some(p);
            }
        }
        if let (Some(bid), Some(ask)) = (self.bids.first(), self.asks.first()) {
            ensure!(decimal(&bid[0])? < decimal(&ask[0])?, "Crossed order book");
        }
        Ok(())
    }
    pub fn limit(&self, sell: bool, quantity: &BigInt, m: &Market) -> Result<Option<String>> {
        let mut remaining = quantity.clone();
        for [p, a] in if sell { &self.bids } else { &self.asks } {
            ensure!(
                units(p, m.price_decimals)? > BigInt::zero(),
                "Invalid price precision"
            );
            let size = units(a, m.base_token_decimals)?;
            ensure!(size > BigInt::zero(), "Invalid size");
            remaining -= size.min(remaining.clone());
            if remaining.is_zero() {
                return Ok(Some(p.clone()));
            }
        }
        Ok(None)
    }
}
