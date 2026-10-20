use crate::numbers::{address, decimal, positive};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use url::Url;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    #[serde(rename = "rpcURL")]
    pub rpc_url: String,
    #[serde(rename = "chainID")]
    pub chain_id: u64,
    pub market: String,
    pub base_token: String,
    pub quote_token: String,
    pub uniswap_v3_pool: String,
    pub vault_address: String,
    pub exchange_address: String,
    pub account: String,
    pub private_key_file: PathBuf,
    pub state_dir: PathBuf,
    pub order_amount: String,
    pub min_profit_pct: String,
    pub min_profit_quote: String,
    pub gas_cost_quote: String,
    pub max_quote_per_trade: String,
    pub max_total_quote: String,
    pub max_loss_quote: String,
    pub max_fee_rate: String,
    pub max_gas_price_gwei: String,
    pub max_hedge_gas_native: String,
    #[serde(rename = "slippageBPS")]
    pub slippage_bps: u32,
    pub confirmations: u64,
    pub poll_milliseconds: u64,
    pub cancel_after_milliseconds: u64,
    #[serde(rename = "orderTTLSeconds")]
    pub order_ttl_seconds: u64,
    pub timeout_seconds: u64,
    pub max_data_age_seconds: u64,
}
impl Config {
    pub fn load(path: &Path, live: bool) -> Result<Self> {
        let meta = fs::metadata(path).context("Cannot read configuration")?;
        ensure!(meta.len() <= 65536, "Configuration exceeds 64 KiB");
        let mut c: Self = serde_json::from_slice(&fs::read(path)?)
            .context("Invalid configuration JSON or field type")?;
        c.validate(live)?;
        let parent = std::env::current_dir()?
            .join(path)
            .parent()
            .unwrap()
            .to_path_buf();
        c.state_dir = parent.join(&c.state_dir);
        if !c.private_key_file.as_os_str().is_empty() {
            c.private_key_file = parent.join(&c.private_key_file);
        }
        Ok(c)
    }
    pub fn validate(&self, live: bool) -> Result<()> {
        for s in [&self.endpoint, &self.rpc_url] {
            let u = Url::parse(s).context("Invalid endpoint")?;
            ensure!(
                u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none(),
                "Endpoint must not include credentials, query, or fragment"
            );
            ensure!(
                u.scheme() == "https"
                    || u.scheme() == "http"
                        && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
                "Use HTTPS or loopback HTTP endpoints"
            );
        }
        ensure!(self.chain_id == 4663, "This example supports chain 4663");
        let symbols: Vec<_> = self.market.split('-').collect();
        ensure!(
            symbols.len() == 2
                && symbols
                    .iter()
                    .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())),
            "Invalid market ID"
        );
        ensure!(
            address(&self.base_token)? != address(&self.quote_token)?,
            "Identical market tokens"
        );
        address(&self.uniswap_v3_pool)?;
        if !self.account.is_empty() {
            address(&self.account)?;
        }
        if live {
            address(&self.vault_address)?;
            address(&self.exchange_address)?;
            ensure!(
                !self.private_key_file.as_os_str().is_empty(),
                "Live mode requires privateKeyFile"
            );
        }
        for s in [
            &self.order_amount,
            &self.min_profit_pct,
            &self.min_profit_quote,
            &self.gas_cost_quote,
            &self.max_quote_per_trade,
            &self.max_total_quote,
            &self.max_loss_quote,
            &self.max_fee_rate,
            &self.max_gas_price_gwei,
            &self.max_hedge_gas_native,
        ] {
            positive(s)?;
        }
        ensure!(
            decimal(&self.min_profit_pct)? < decimal("1")?
                && decimal(&self.max_fee_rate)? < decimal("1")?,
            "Invalid profit or fee fraction"
        );
        ensure!(
            decimal(&self.max_quote_per_trade)? <= decimal(&self.max_total_quote)?,
            "Per-trade cap exceeds total budget"
        );
        ensure!(!self.state_dir.as_os_str().is_empty(), "Missing stateDir");
        for (value, low, high, name) in [
            (self.slippage_bps as u64, 1, 1000, "slippageBPS"),
            (self.confirmations, 1, 100, "confirmations"),
            (self.poll_milliseconds, 250, 60000, "pollMilliseconds"),
            (
                self.cancel_after_milliseconds,
                250,
                10000,
                "cancelAfterMilliseconds",
            ),
            (self.order_ttl_seconds, 60, 120, "orderTTLSeconds"),
            (self.timeout_seconds, 1, 30, "timeoutSeconds"),
            (self.max_data_age_seconds, 2, 60, "maxDataAgeSeconds"),
        ] {
            ensure!((low..=high).contains(&value), "Invalid {name}");
        }
        Ok(())
    }
}
