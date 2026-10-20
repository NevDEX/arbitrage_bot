use crate::{config::Config, numbers::*};
use alloy::{
    primitives::{Address, Bytes},
    sol_types::SolCall,
};
use anyhow::{Result, bail, ensure};
use futures_util::StreamExt;
use reqwest::{Client, redirect::Policy};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const RESPONSE_LIMIT: usize = 4 << 20;
fn client(c: &Config) -> Result<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(c.timeout_seconds))
        .redirect(Policy::none())
        .build()?)
}
async fn post(client: &Client, url: &str, payload: &impl Serialize, label: &str) -> Result<Value> {
    let response = client
        .post(url)
        .json(payload)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("{label} unavailable"))?;
    ensure!(
        response.status().as_u16() == 200,
        "{label} HTTP status={}",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("{label} response read failed"))?;
        ensure!(
            bytes.len() + chunk.len() <= RESPONSE_LIMIT,
            "{label} response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("{label} invalid JSON"))
}
/// Read and mutation requests share a client; place/cancel requests are serialized.
#[derive(Clone)]
pub struct Api {
    client: Client,
    endpoint: String,
    last_mutation: Arc<Mutex<Option<Instant>>>,
}
impl Api {
    pub fn new(c: &Config) -> Result<Self> {
        Ok(Self {
            client: client(c)?,
            endpoint: c.endpoint.trim_end_matches('/').into(),
            last_mutation: Arc::new(Mutex::new(None)),
        })
    }
    pub async fn call(&self, path: &str, payload: Value, required: bool) -> Result<Value> {
        ensure!(
            matches!(
                path,
                "getMarket"
                    | "order/orderbook"
                    | "lockedBalance"
                    | "order/build"
                    | "order/place"
                    | "order/cancel"
            ),
            "Unsupported API path"
        );
        let mut pacing = if matches!(path, "order/place" | "order/cancel") {
            Some(self.last_mutation.lock().await)
        } else {
            None
        };
        if let Some(last) = pacing.as_ref().and_then(|g| **g) {
            tokio::time::sleep(Duration::from_millis(500).saturating_sub(last.elapsed())).await;
        }
        let result = post(
            &self.client,
            &format!("{}/{path}", self.endpoint),
            &payload,
            &format!("API {path}"),
        )
        .await;
        if let Some(g) = pacing.as_mut() {
            **g = Some(Instant::now());
        }
        let result = result?;
        ensure!(
            result
                .get("code")
                .is_some_and(|n| n == 20000 || n == "20000"),
            "API {path} rejected request"
        );
        let data = result.get("data").cloned().unwrap_or(Value::Null);
        ensure!(!required || !data.is_null(), "API {path} returned no data");
        Ok(data)
    }
    pub async fn get<T: DeserializeOwned>(&self, path: &str, payload: Value) -> Result<T> {
        serde_json::from_value(self.call(path, payload, true).await?)
            .map_err(|_| anyhow::anyhow!("API {path} returned invalid fields"))
    }
    pub async fn cancel(&self, market: &str, address: &str, id: &str) -> Result<()> {
        self.call(
            "order/cancel",
            json!({"marketID":market,"address":address,"orderID":id}),
            false,
        )
        .await?;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Rpc {
    client: Client,
    url: String,
    id: Arc<AtomicU64>,
    max_age: u64,
}
#[derive(Clone, Debug)]
pub struct Block {
    pub number: u64,
    pub timestamp: u64,
    pub hash: String,
    pub tag: String,
}
impl Rpc {
    pub fn new(c: &Config) -> Result<Self> {
        Ok(Self {
            client: client(c)?,
            url: c.rpc_url.clone(),
            id: Arc::new(AtomicU64::new(0)),
            max_age: c.max_data_age_seconds,
        })
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id.fetch_add(1, Ordering::Relaxed) + 1;
        let value = post(
            &self.client,
            &self.url,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            &format!("RPC {method}"),
        )
        .await?;
        ensure!(
            value["jsonrpc"] == "2.0"
                && value["id"] == id
                && value.get("error").is_none_or(Value::is_null)
                && value.get("result").is_some(),
            "RPC {method} failed"
        );
        Ok(value["result"].clone())
    }
    pub async fn quantity(&self, method: &str, params: Value) -> Result<u64> {
        let v = self.call(method, params).await?;
        hex_u64(
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid RPC quantity"))?,
        )
    }
    pub async fn amount(&self, method: &str, params: Value) -> Result<num_bigint::BigInt> {
        let v = self.call(method, params).await?;
        let s = v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid RPC amount"))?;
        ensure!(s.starts_with("0x") && s.len() <= 66, "Invalid RPC amount");
        num_bigint::BigInt::parse_bytes(&s.as_bytes()[2..], 16)
            .ok_or_else(|| anyhow::anyhow!("Invalid RPC amount"))
    }
    pub async fn read<C: SolCall>(&self, to: Address, call: C, tag: &str) -> Result<C::Return> {
        let data = Bytes::from(call.abi_encode());
        let v = self
            .call("eth_call", json!([{"to":to,"data":data},tag]))
            .await?;
        let data: Bytes =
            serde_json::from_value(v).map_err(|_| anyhow::anyhow!("Invalid contract result"))?;
        C::abi_decode_returns_validate(&data)
            .map_err(|_| anyhow::anyhow!("Invalid ABI result for {}", C::SIGNATURE))
    }
    pub async fn block(&self, tag: &str) -> Result<Block> {
        let b = self
            .call("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        let get = |k| {
            b.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Invalid chain block"))
        };
        hash(get("hash")?)?;
        let block = Block {
            number: hex_u64(get("number")?)?,
            timestamp: hex_u64(get("timestamp")?)?,
            hash: get("hash")?.into(),
            tag: get("number")?.into(),
        };
        if tag != "latest" && hex_u64(tag)? != block.number {
            bail!("RPC returned wrong block");
        }
        Ok(block)
    }
    pub async fn head(&self) -> Result<Block> {
        let b = self.block("latest").await?;
        let now = now_ms() / 1000;
        ensure!(
            b.timestamp > 0
                && b.timestamp <= now + 5
                && now.saturating_sub(b.timestamp) <= self.max_age,
            "Stale RPC head or incorrect clock"
        );
        Ok(b)
    }
}
