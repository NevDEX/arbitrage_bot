use crate::{
    config::Config,
    market::Market,
    numbers::*,
    planner::{Quote, Side},
};
use alloy::{
    primitives::{B256, hex, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
};
use anyhow::{Result, ensure};
use num_traits::Zero;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltOrder {
    pub id: String,
    #[serde(rename = "marketID")]
    pub market_id: String,
    pub side: Side,
    #[serde(rename = "type")]
    pub order_type: String,
    #[serde(deserialize_with = "wire_string")]
    pub price: String,
    #[serde(deserialize_with = "wire_string")]
    pub amount: String,
    #[serde(deserialize_with = "wire_string")]
    pub gas_fee_amount: String,
    pub json: OrderData,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderData {
    pub trader: String,
    pub relayer: String,
    pub base_token: String,
    pub quote_token: String,
    pub base_token_amount: String,
    pub quote_token_amount: String,
    pub gas_token_amount: String,
    pub data: String,
}
pub fn built_hash(o: &BuiltOrder) -> Result<B256> {
    let j = &o.json;
    let mut words = keccak256("Order(address trader,address relayer,address baseToken,address quoteToken,uint256 baseTokenAmount,uint256 quoteTokenAmount,uint256 gasTokenAmount,bytes32 data)").to_vec();
    for s in [&j.trader, &j.relayer, &j.base_token, &j.quote_token] {
        words.extend_from_slice(&address(s)?.into_word().0);
    }
    for s in [
        &j.base_token_amount,
        &j.quote_token_amount,
        &j.gas_token_amount,
    ] {
        words.extend_from_slice(&uint(&unsigned(s)?)?.to_be_bytes::<32>());
    }
    let data: B256 = j
        .data
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid signed data"))?;
    words.extend_from_slice(data.as_slice());
    let mut domain = keccak256("EIP712Domain(string name)").to_vec();
    domain.extend_from_slice(keccak256("NevDEX").as_slice());
    let mut digest = vec![0x19, 0x01];
    digest.extend_from_slice(keccak256(domain).as_slice());
    digest.extend_from_slice(keccak256(words).as_slice());
    Ok(keccak256(digest))
}
pub fn validate_built(
    o: &BuiltOrder,
    q: &Quote,
    c: &Config,
    m: &Market,
    owner: &str,
    timestamp: u64,
) -> Result<u64> {
    ensure!(hash(&o.id)? == built_hash(o)?, "Built order hash mismatch");
    ensure!(
        o.market_id == c.market
            && o.side == q.side
            && o.order_type == "limit"
            && decimal(&o.price)? == decimal(&q.price)?
            && decimal(&o.amount)? == decimal(&q.amount)?
            && decimal(&o.gas_fee_amount)?.is_zero(),
        "Build response changed requested order"
    );
    let j = &o.json;
    ensure!(
        same(&j.trader, owner)
            && same(&j.base_token, &c.base_token)
            && same(&j.quote_token, &c.quote_token)
            && unsigned(&j.base_token_amount)? == units(&q.amount, m.base_token_decimals)?
            && unsigned(&j.quote_token_amount)?
                == notional(&q.price, &q.amount, m.quote_token_decimals)?
            && unsigned(&j.gas_token_amount)?.is_zero(),
        "Build response changed tokens or amounts"
    );
    let data: B256 = j.data.parse()?;
    ensure!(
        data[0] == 2
            && data[1] == u8::from(q.side == Side::Sell)
            && data[2] == 0
            && data[12] == 0
            && data[13] == 0
            && data[22] == 0,
        "Unexpected order version, direction, type, rebate or post-only flag"
    );
    let expiry = data[3..8].iter().fold(0u64, |n, b| (n << 8) | *b as u64);
    ensure!(
        expiry >= timestamp + 30 && expiry <= timestamp + c.order_ttl_seconds + 10,
        "Unsafe signed order expiry"
    );
    for (i, rate) in [&m.maker_fee_rate, &m.taker_fee_rate].iter().enumerate() {
        ensure!(
            num_bigint::BigInt::from(u16::from_be_bytes([data[8 + i * 2], data[9 + i * 2]]))
                == units(rate, 5)?,
            "Signed fee differs from market fee"
        );
    }
    Ok(expiry)
}
/// Sign the raw order ID with EIP-191, then encode NevDEX's v/padding/r/s layout.
pub fn sign_order(signer: &PrivateKeySigner, id: &str) -> Result<String> {
    let sig = signer.sign_message_sync(hash(id)?.as_slice())?;
    let mut out = [0u8; 96];
    out[0] = 27 + u8::from(sig.v());
    out[32..64].copy_from_slice(&sig.r().to_be_bytes::<32>());
    out[64..].copy_from_slice(&sig.s().to_be_bytes::<32>());
    Ok(format!("0x{}", hex::encode(out)))
}
#[derive(Clone, Debug, Deserialize)]
pub struct OpenOrder {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "TraderAddress")]
    pub trader: String,
    #[serde(rename = "MarketId")]
    pub market: String,
    #[serde(rename = "Side")]
    pub side: Side,
    #[serde(rename = "Type")]
    pub order_type: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "Price", deserialize_with = "wire_string")]
    pub price: String,
    #[serde(rename = "Amount", deserialize_with = "wire_string")]
    pub amount: String,
    #[serde(rename = "AvailableAmount", deserialize_with = "wire_string")]
    pub available: String,
    #[serde(rename = "PendingAmount", deserialize_with = "wire_string")]
    pub pending: String,
}
pub fn validate_open(orders: &[OpenOrder], c: &Config, owner: &str) -> Result<()> {
    ensure!(orders.len() <= 10000, "Too many open orders");
    let mut seen = std::collections::HashSet::new();
    for o in orders {
        ensure!(
            o.market == c.market
                && same(&o.trader, owner)
                && seen.insert(hash(&o.id)?)
                && matches!(o.order_type.as_str(), "limit" | "market")
                && matches!(o.status.as_str(), "pending" | "settling"),
            "Open order identity or status mismatch"
        );
        positive(&o.price)?;
        positive(&o.amount)?;
        ensure!(
            decimal(&o.available)? + decimal(&o.pending)? <= decimal(&o.amount)?,
            "Invalid open order quantities"
        );
    }
    Ok(())
}
