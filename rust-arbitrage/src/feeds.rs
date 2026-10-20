use crate::{
    config::Config,
    market::Depth,
    numbers::*,
    orders::{OpenOrder, validate_open},
    transport::Api,
};
use anyhow::{Result, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub struct BookSnapshot {
    pub depth: Depth,
    pub revision: u64,
    pub contact: Instant,
}
#[derive(Clone)]
pub struct AccountSnapshot {
    pub orders: Vec<OpenOrder>,
    pub block: u64,
    pub version: u64,
    pub contact: Instant,
}
#[derive(Default)]
struct Shared {
    book: Option<BookSnapshot>,
    account: Option<AccountSnapshot>,
    revision: u64,
    account_block: u64,
}
#[derive(Clone)]
pub struct Feeds {
    shared: Arc<RwLock<Shared>>,
    max_age: Duration,
}
impl Feeds {
    pub fn new(c: &Config) -> Self {
        Self {
            shared: Arc::new(RwLock::new(Shared::default())),
            max_age: Duration::from_secs(c.max_data_age_seconds),
        }
    }
    pub fn book(&self) -> Option<BookSnapshot> {
        self.shared
            .read()
            .unwrap()
            .book
            .clone()
            .filter(|s| s.contact.elapsed() <= self.max_age)
    }
    pub fn account(&self) -> Option<AccountSnapshot> {
        self.shared
            .read()
            .unwrap()
            .account
            .clone()
            .filter(|s| s.contact.elapsed() <= self.max_age)
    }
    fn invalidate(&self, account: bool) {
        let mut s = self.shared.write().unwrap();
        if account {
            s.account = None;
        } else {
            s.book = None;
            s.revision += 1;
        }
    }
    fn depth(&self, raw: Value) -> Result<()> {
        let depth: Depth = serde_json::from_value(raw)?;
        depth.validate()?;
        let mut s = self.shared.write().unwrap();
        if s.book
            .as_ref()
            .is_some_and(|b| depth.sequence < b.depth.sequence)
        {
            return Ok(());
        }
        s.revision += 1;
        s.book = Some(BookSnapshot {
            depth,
            revision: s.revision,
            contact: Instant::now(),
        });
        Ok(())
    }
    fn account_update(&self, raw: Value, c: &Config, owner: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Snapshot {
            address: String,
            #[serde(deserialize_with = "wire_u64")]
            version: u64,
            #[serde(rename = "blockNumber", deserialize_with = "wire_u64")]
            block: u64,
            #[serde(rename = "openOrder")]
            orders: Vec<OpenOrder>,
        }
        let n: Snapshot = serde_json::from_value(raw)?;
        ensure!(
            same(&n.address, owner) && n.version > 0,
            "Invalid account snapshot identity"
        );
        validate_open(&n.orders, c, owner)?;
        let mut s = self.shared.write().unwrap();
        ensure!(
            s.account.as_ref().is_none_or(|a| n.version > a.version),
            "Out-of-order account snapshot"
        );
        s.account_block = s.account_block.max(n.block);
        s.account = Some(AccountSnapshot {
            orders: n.orders,
            block: s.account_block,
            version: n.version,
            contact: Instant::now(),
        });
        Ok(())
    }
    fn ping(&self, account: bool) -> Result<()> {
        let mut s = self.shared.write().unwrap();
        if account {
            if let Some(a) = s.account.as_mut() {
                ensure!(a.contact.elapsed() <= self.max_age, "Account heartbeat gap");
                a.contact = Instant::now();
            }
        } else if let Some(b) = s.book.as_mut() {
            b.contact = Instant::now();
        }
        Ok(())
    }
    pub async fn watch(&self, c: Config, api: Api, owner: Option<String>, stop: CancellationToken) {
        let account = owner.is_some();
        let mut delay = 1;
        while !stop.is_cancelled() {
            self.invalidate(account);
            let session = self.session(&c, &api, owner.as_deref(), &stop);
            tokio::select! { _ = stop.cancelled() => break, _ = session => {} }
            self.invalidate(account);
            tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(delay)) => {} }
            delay = (delay * 2).min(8);
        }
        self.invalidate(account);
    }
    async fn session(
        &self,
        c: &Config,
        api: &Api,
        owner: Option<&str>,
        stop: &CancellationToken,
    ) -> Result<()> {
        let mut url = Url::parse(&format!(
            "{}/{}",
            c.endpoint.trim_end_matches('/'),
            if owner.is_some() {
                "v1/orders/ws"
            } else {
                "v1/ws"
            }
        ))?;
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .unwrap();
        url.query_pairs_mut().append_pair("marketID", &c.market);
        if let Some(owner) = owner {
            url.query_pairs_mut()
                .append_pair("address", owner)
                .append_pair("openOnly", "1");
        }
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(4 << 20))
            .max_frame_size(Some(4 << 20));
        let (mut socket, _) = tokio::time::timeout(
            Duration::from_secs(c.timeout_seconds),
            connect_async_with_config(url.as_str(), Some(ws_config), false),
        )
        .await??;
        let bootstrap = async {
            if owner.is_none() {
                api.call("order/orderbook", json!({"marketID":c.market}), true)
                    .await
            } else {
                std::future::pending::<Result<Value>>().await
            }
        };
        tokio::pin!(bootstrap);
        let mut bootstrapped = false;
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                raw = &mut bootstrap, if !bootstrapped => {
                    bootstrapped = true;
                    if let Ok(raw) = raw { self.depth(raw)?; }
                },
                frame = tokio::time::timeout(Duration::from_secs(45),socket.next()) => {
                    match frame?.ok_or_else(|| anyhow::anyhow!("Stream closed"))?? {
                        Message::Text(text) => {
                            let event:Value = serde_json::from_str(&text)?;
                            if let Some(owner) = owner {
                                ensure!(event["marketID"] == c.market && event["type"] == "orders_snapshot","Unexpected account event");
                                self.account_update(event["data"].clone(),c,owner)?;
                            } else if event["marketID"] == c.market && event["type"] == "depth_update" {
                                self.depth(event["data"].clone())?;
                            }
                        },
                        Message::Ping(data) => {
                            self.ping(owner.is_some())?;
                            tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Pong(data))).await??;
                        },
                        Message::Pong(_) => {},
                        _ => anyhow::bail!("Unsupported or closed stream frame"),
                    }
                }
            }
        }
    }
}
