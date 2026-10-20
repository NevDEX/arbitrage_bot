//! Local protocol fixtures; no requests are made to external endpoints.
use alloy::{
    primitives::{
        Bytes, U256,
        aliases::{U24, U160},
    },
    sol_types::{SolCall, SolEvent},
};
use futures_util::{SinkExt, StreamExt};
use nevdex_arbitrage::{
    abi::*,
    config::Config,
    engine::{Engine, Options},
    market::Market,
    numbers::*,
    orders::{BuiltOrder, built_hash},
    planner::{Quote, Side},
    state::{Intent, Journal},
    transport::Rpc,
    uniswap::{FACTORY, QUOTER, Uniswap},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

const OWNER: &str = "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf";
const VAULT: &str = "0x3333333333333333333333333333333333333333";
const EXCHANGE: &str = "0x4444444444444444444444444444444444444444";
fn block_hash(n: u64) -> String {
    format!("0x{n:064x}")
}
fn market(c: &Config) -> Market {
    serde_json::from_value(market_json(c)).unwrap()
}
fn market_json(c: &Config) -> Value {
    json!({"id":c.market,"isPublished":true,"baseTokenAddress":c.base_token,"quoteTokenAddress":c.quote_token,
        "baseTokenDecimals":18,"quoteTokenDecimals":18,"priceDecimals":4,"amountDecimals":4,"minOrderSize":"0","makerFeeRate":"0.001","takerFeeRate":"0.002"})
}
#[derive(Default)]
struct MockState {
    head: u64,
    requests: Vec<String>,
    placed: Option<String>,
    logs: Vec<Value>,
    receipt: Value,
    wrong_chain: bool,
    reorg: bool,
    journal_path: Option<std::path::PathBuf>,
    broadcast_hash: Option<String>,
}
struct Mock {
    config: Config,
    state: Arc<Mutex<MockState>>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
impl Mock {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config: Config =
            serde_json::from_str(include_str!("../arbitrage.example.json")).unwrap();
        let port = listener.local_addr().unwrap().port();
        config.endpoint = format!("http://127.0.0.1:{port}/api/");
        config.rpc_url = format!("http://127.0.0.1:{port}/rpc");
        config.poll_milliseconds = 250;
        config.confirmations = 1;
        config.vault_address = VAULT.into();
        config.exchange_address = EXCHANGE.into();
        let state = Arc::new(Mutex::new(MockState {
            head: 100,
            ..Default::default()
        }));
        let stop = CancellationToken::new();
        let (s, c, t) = (state.clone(), config.clone(), stop.clone());
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=t.cancelled()=>break,
                    accepted=listener.accept()=>{
                        let (socket,_)=accepted.unwrap(); let (s,c,t)=(s.clone(),c.clone(),t.clone());
                        tokio::spawn(async move { tokio::select! { _=t.cancelled()=>{}, _=connection(socket,s,c)=>{} } });
                    }
                }
            }
        });
        Self {
            config,
            state,
            stop,
            task,
        }
    }
}
async fn connection(mut socket: TcpStream, state: Arc<Mutex<MockState>>, c: Config) {
    let mut peek = [0u8; 4096];
    if socket.peek(&mut peek).await.unwrap_or(0) == 0 {
        return;
    }
    if peek.starts_with(b"GET") {
        let account = String::from_utf8_lossy(&peek).contains("/orders/ws");
        let mut ws = accept_async(socket).await.unwrap();
        let event = if account {
            json!({"marketID":c.market,"type":"orders_snapshot","data":{"address":OWNER,"version":1,"blockNumber":0,"openOrder":[]}})
        } else {
            json!({"marketID":c.market,"type":"depth_update","data":{"sequence":1,"bids":[["1","1"]],"asks":[["2","1"]]}})
        };
        if ws
            .send(Message::Text(event.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
        while let Some(Ok(_)) = ws.next().await {}
        return;
    }
    let mut request = Vec::new();
    let end = loop {
        let mut chunk = [0u8; 4096];
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..n]);
        if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(request.len() < 65536);
    };
    let header = String::from_utf8_lossy(&request[..end]);
    let path = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let length: usize = header
        .lines()
        .find_map(|s| {
            s.to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse().unwrap())
        })
        .unwrap();
    while request.len() < end + length {
        let mut b = [0; 4096];
        let n = socket.read(&mut b).await.unwrap();
        if n == 0 {
            return;
        }
        request.extend_from_slice(&b[..n]);
    }
    let payload: Value = serde_json::from_slice(&request[end..end + length]).unwrap();
    let response = respond(&path, payload, &mut state.lock().unwrap(), &c);
    let body = response.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
}
fn abi_response<C: SolCall>(r: &C::Return) -> Value {
    json!(Bytes::from(C::abi_encode_returns(r)))
}
fn respond(path: &str, p: Value, s: &mut MockState, c: &Config) -> Value {
    if path == "/rpc" {
        let method = p["method"].as_str().unwrap();
        s.requests.push(method.into());
        let params = &p["params"];
        let result = match method {
            "eth_chainId" => json!(if s.wrong_chain { "0x1" } else { "0x1237" }),
            "eth_getBlockByNumber" => {
                let tag = params[0].as_str().unwrap();
                let n = if tag == "latest" {
                    s.head += 1;
                    s.head
                } else {
                    hex_u64(tag).unwrap()
                };
                json!({"number":format!("0x{n:x}"),"hash":block_hash(if s.reorg { n+1 } else { n }),"timestamp":format!("0x{:x}",now_ms()/1000)})
            }
            "eth_blockNumber" => json!(format!("0x{:x}", s.head)),
            "eth_getCode" => json!("0x6000"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_gasPrice" => json!("0x3b9aca00"),
            "eth_getTransactionCount" => json!("0x7"),
            "eth_sendRawTransaction" => {
                let bytes: Bytes = serde_json::from_value(params[0].clone()).unwrap();
                let tx_hash = alloy::primitives::keccak256(bytes).to_string();
                let state: Value = serde_json::from_slice(
                    &std::fs::read(
                        s.journal_path
                            .as_ref()
                            .expect("Unexpected broadcast without journal"),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(state["hedge_tx"]["hash"], tx_hash);
                assert_eq!(state["hedge_tx"]["nonce"], 7);
                s.broadcast_hash = Some(tx_hash.clone());
                json!(tx_hash)
            }
            "eth_getLogs" => {
                let from = hex_u64(params[0]["fromBlock"].as_str().unwrap()).unwrap();
                let to = hex_u64(params[0]["toBlock"].as_str().unwrap()).unwrap();
                json!(
                    s.logs
                        .iter()
                        .filter(|l| {
                            let n = hex_u64(l["blockNumber"].as_str().unwrap()).unwrap();
                            n >= from && n <= to
                        })
                        .collect::<Vec<_>>()
                )
            }
            "eth_getTransactionReceipt" => s.receipt.clone(),
            "eth_call" => {
                let data: Bytes = serde_json::from_value(params[0]["data"].clone()).unwrap();
                let selector = &data[..4];
                if selector == token0Call::SELECTOR {
                    abi_response::<token0Call>(&address(&c.base_token).unwrap())
                } else if selector == token1Call::SELECTOR {
                    abi_response::<token1Call>(&address(&c.quote_token).unwrap())
                } else if selector == factoryCall::SELECTOR {
                    abi_response::<factoryCall>(&address(FACTORY).unwrap())
                } else if selector == feeCall::SELECTOR {
                    abi_response::<feeCall>(&U24::from(3000))
                } else if selector == getPoolCall::SELECTOR {
                    abi_response::<getPoolCall>(&address(&c.uniswap_v3_pool).unwrap())
                } else if selector == decimalsCall::SELECTOR {
                    abi_response::<decimalsCall>(&18)
                } else if selector == liquidityCall::SELECTOR {
                    abi_response::<liquidityCall>(&1)
                } else if selector == slot0Call::SELECTOR {
                    abi_response::<slot0Call>(&slot0Return {
                        sqrtPriceX96: U160::from(1u128 << 96),
                        tick: Default::default(),
                        observationIndex: 0,
                        observationCardinality: 1,
                        observationCardinalityNext: 1,
                        feeProtocol: 0,
                        unlocked: true,
                    })
                } else if selector == vaultCall::SELECTOR {
                    abi_response::<vaultCall>(&address(VAULT).unwrap())
                } else if selector == exchangeCall::SELECTOR {
                    abi_response::<exchangeCall>(&address(EXCHANGE).unwrap())
                } else if selector == balancesCall::SELECTOR
                    || selector == balanceOfCall::SELECTOR
                    || selector == allowanceCall::SELECTOR
                {
                    abi_response::<balanceOfCall>(&U256::from(100000000000000000000u128))
                } else if selector == quoteExactInputSingleCall::SELECTOR {
                    abi_response::<quoteExactInputSingleCall>(&quoteExactInputSingleReturn {
                        amountOut: U256::from(5000000000000000u64),
                        sqrtPriceX96After: U160::from(1u128 << 96),
                        initializedTicksCrossed: 0,
                        gasEstimate: U256::from(1),
                    })
                } else if selector == quoteExactOutputSingleCall::SELECTOR {
                    abi_response::<quoteExactOutputSingleCall>(&quoteExactOutputSingleReturn {
                        amountIn: U256::from(40000000000000000u64),
                        sqrtPriceX96After: U160::from(1u128 << 96),
                        initializedTicksCrossed: 0,
                        gasEstimate: U256::from(1),
                    })
                } else if selector == multicallCall::SELECTOR {
                    json!("0x")
                } else {
                    panic!("Unexpected eth_call selector {selector:?}");
                }
            }
            _ => panic!("Unexpected RPC mutation or method: {method}"),
        };
        return json!({"jsonrpc":"2.0","id":p["id"],"result":result});
    }
    s.requests.push(path.into());
    let data = match path {
        "/api/getMarket" => market_json(c),
        "/api/order/orderbook" => json!({"sequence":1,"bids":[["1","1"]],"asks":[["2","1"]]}),
        "/api/lockedBalance" => json!({"CASHCAT":"0","USDG":"0"}),
        "/api/order/build" => {
            let mut fixture: Value =
                serde_json::from_str(include_str!("fixtures/order.json")).unwrap();
            let o = &mut fixture["order"];
            let expiry = (now_ms() / 1000) + 60;
            let mut data = [0u8; 32];
            data[0] = 2;
            data[1] = 1;
            data[3..8].copy_from_slice(&expiry.to_be_bytes()[3..]);
            data[9] = 100;
            data[11] = 200;
            o["price"] = p["price"].clone();
            o["amount"] = p["amount"].clone();
            o["json"]["data"] = json!(format!("0x{}", alloy::hex::encode(data)));
            o["json"]["quoteTokenAmount"] = json!(
                notional(
                    p["price"].as_str().unwrap(),
                    p["amount"].as_str().unwrap(),
                    18
                )
                .unwrap()
                .to_string()
            );
            let built: BuiltOrder = serde_json::from_value(o.clone()).unwrap();
            o["id"] = json!(built_hash(&built).unwrap());
            o.clone()
        }
        "/api/order/place" => {
            s.placed = Some(p["orderID"].as_str().unwrap().into());
            Value::Null
        }
        "/api/order/cancel" => Value::Null,
        _ => panic!("Unexpected API path {path}"),
    };
    json!({"code":20000,"data":data})
}

#[tokio::test]
async fn read_only_check_and_preview_never_open_keys_or_mutate() {
    let mock = Mock::start().await;
    let temp = tempfile::tempdir().unwrap();
    for options in [
        Options {
            check: true,
            ..Default::default()
        },
        Options {
            once: true,
            ..Default::default()
        },
    ] {
        let mut c = mock.config.clone();
        c.private_key_file = temp.path().join("missing.key");
        c.state_dir = temp.path().join("must-not-exist");
        Engine::new(c.clone())
            .unwrap()
            .run(options, CancellationToken::new())
            .await
            .unwrap();
        assert!(!c.state_dir.exists());
    }
    let s = mock.state.lock().unwrap();
    assert!(!s.requests.iter().any(|r| matches!(
        r.as_str(),
        "/api/order/build" | "/api/order/place" | "/api/order/cancel" | "eth_sendRawTransaction"
    )));
}
#[tokio::test]
async fn wrong_chain_stops_before_execution() {
    let mock = Mock::start().await;
    mock.state.lock().unwrap().wrong_chain = true;
    let error = Engine::new(mock.config.clone())
        .unwrap()
        .run(
            Options {
                check: true,
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("chain mismatch"));
}
#[tokio::test]
#[cfg(unix)]
async fn queued_order_is_persisted_once_and_never_hedged_without_fill() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let mock = Mock::start().await;
    let temp = tempfile::tempdir().unwrap();
    let mut c = mock.config.clone();
    c.state_dir = temp.path().join("state");
    c.private_key_file = temp.path().join("public-test.key");
    fs::write(&c.private_key_file, format!("{:064x}", 1)).unwrap();
    fs::set_permissions(&c.private_key_file, fs::Permissions::from_mode(0o600)).unwrap();
    let stop = CancellationToken::new();
    let t = stop.clone();
    let config = c.clone();
    let engine = tokio::spawn(async move {
        Engine::new(config)
            .unwrap()
            .run(
                Options {
                    live: true,
                    ..Default::default()
                },
                t,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if mock.state.lock().unwrap().placed.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    stop.cancel();
    engine.await.unwrap().unwrap();
    let s = mock.state.lock().unwrap();
    assert_eq!(
        s.requests
            .iter()
            .filter(|r| r.as_str() == "/api/order/place")
            .count(),
        1
    );
    assert!(!s.requests.iter().any(|r| r == "eth_sendRawTransaction"));
    let j = Journal::open(&c, &market(&c), OWNER).unwrap();
    assert_eq!(
        j.state.active.as_ref().unwrap().id,
        s.placed.as_ref().unwrap().as_str()
    );
    assert!(j.state.pending.is_empty());
    assert!(j.state.hedge_tx.is_none());
}

#[tokio::test]
#[cfg(unix)]
async fn confirmed_partial_fill_is_deduplicated_and_failed_hedge_retains_it() {
    let mock = Mock::start().await;
    let temp = tempfile::tempdir().unwrap();
    let mut c = mock.config.clone();
    c.state_dir = temp.path().join("state");
    let m = market(&c);
    let rpc = Rpc::new(&c).unwrap();
    let head = rpc.head().await.unwrap();
    let uni = Uniswap::checked(c.clone(), m.clone(), rpc.clone(), &head, false)
        .await
        .unwrap();
    let mut j = Journal::open(&c, &m, OWNER).unwrap();
    j.state.cursor = 95;
    j.state.cursor_hash = block_hash(95);
    j.state.active = Some(Intent {
        id: block_hash(123),
        relayer: address(QUOTER).unwrap().to_string(),
        quote: Quote {
            side: Side::Sell,
            price: "1".into(),
            amount: "0.1".into(),
        },
        expires: now_ms() / 1000 + 60,
        created_at: now_ms(),
        seen: true,
        last_cancel_at: 0,
        filled_base_raw: "0".into(),
    });
    let event = Match {
        addressSet: AddressSet {
            baseToken: address(&c.base_token).unwrap(),
            quoteToken: address(&c.quote_token).unwrap(),
            relayer: address(QUOTER).unwrap(),
        },
        result: MatchResult {
            maker: address(OWNER).unwrap(),
            taker: address(VAULT).unwrap(),
            buyer: address(VAULT).unwrap(),
            makerFee: U256::ZERO,
            makerRebate: U256::ZERO,
            takerFee: U256::ZERO,
            makerGasFee: U256::ZERO,
            takerGasFee: U256::ZERO,
            baseTokenFilledAmount: U256::from(50),
            quoteTokenFilledAmount: U256::from(100),
        },
    };
    let log = json!({"address":EXCHANGE,"topics":[Match::SIGNATURE_HASH],"data":Bytes::from(event.encode_data()),"transactionHash":block_hash(555),"logIndex":"0x2","blockNumber":"0x60","blockHash":block_hash(96)});
    mock.state.lock().unwrap().logs = vec![log.clone(), log];
    uni.scan(&mut j, address(OWNER).unwrap(), &head)
        .await
        .unwrap();
    uni.scan(&mut j, address(OWNER).unwrap(), &head)
        .await
        .unwrap();
    assert_eq!(j.state.pending.len(), 1);
    assert_eq!(j.state.active.as_ref().unwrap().filled_base_raw, "50");
    j.state.hedge_tx = Some(nevdex_arbitrage::state::HedgeTx {
        hash: block_hash(777),
        nonce: 5,
        deadline: now_ms() / 1000 + 60,
        fill_id: j.state.pending[0].id.clone(),
        bound_raw: "100".into(),
        gas_budget_raw: "1".into(),
    });
    j.save().unwrap();
    assert!(
        !uni.settle(&mut j, address(OWNER).unwrap(), &head)
            .await
            .unwrap()
    );
    mock.state.lock().unwrap().receipt = json!({"transactionHash":block_hash(777),"status":"0x0"});
    assert!(
        uni.settle(&mut j, address(OWNER).unwrap(), &head)
            .await
            .is_err()
    );
    assert_eq!(j.state.pending.len(), 1);
    mock.state.lock().unwrap().reorg = true;
    assert!(
        uni.scan(&mut j, address(OWNER).unwrap(), &head)
            .await
            .is_err()
    );
    drop(j);
    let recovered = Journal::open(&c, &m, OWNER).unwrap();
    assert_eq!(recovered.state.pending.len(), 1);
    assert!(recovered.state.hedge_tx.is_some());
}

#[tokio::test]
#[cfg(unix)]
async fn hedge_persists_before_broadcast_and_settles_actual_transfers_after_restart() {
    use nevdex_arbitrage::state::Fill;
    let mock = Mock::start().await;
    let temp = tempfile::tempdir().unwrap();
    let mut c = mock.config.clone();
    c.state_dir = temp.path().join("state");
    let m = market(&c);
    let rpc = Rpc::new(&c).unwrap();
    let head = rpc.head().await.unwrap();
    let uni = Uniswap::checked(c.clone(), m.clone(), rpc, &head, false)
        .await
        .unwrap();
    let mut j = Journal::open(&c, &m, OWNER).unwrap();
    j.state.cursor = 100;
    j.state.cursor_hash = block_hash(100);
    j.state.active = Some(Intent {
        id: block_hash(123),
        relayer: QUOTER.into(),
        quote: Quote {
            side: Side::Sell,
            price: "1".into(),
            amount: "0.1".into(),
        },
        expires: now_ms() / 1000 + 60,
        created_at: now_ms(),
        seen: true,
        last_cancel_at: 0,
        filled_base_raw: "50000000000000000".into(),
    });
    j.state.pending.push(Fill {
        id: format!("{}:1", block_hash(555)),
        hedge_side: Side::Buy,
        base_raw: "50000000000000000".into(),
        quote_delta_raw: "50000000000000000".into(),
        block: 99,
        block_hash: block_hash(99),
    });
    j.save().unwrap();
    mock.state.lock().unwrap().journal_path =
        Some(c.state_dir.join(OWNER.to_lowercase()).join("state.json"));
    let signer: alloy::signers::local::PrivateKeySigner = format!("{:064x}", 1).parse().unwrap();
    uni.submit(&mut j, &signer, &head).await.unwrap();
    let hash = j.state.hedge_tx.as_ref().unwrap().hash.clone();
    assert_eq!(
        mock.state.lock().unwrap().broadcast_hash.as_deref(),
        Some(hash.as_str())
    );
    drop(j);
    let mut recovered = Journal::open(&c, &m, OWNER).unwrap();
    assert!(uni.submit(&mut recovered, &signer, &head).await.is_err());
    let base_in = Transfer {
        from: address(VAULT).unwrap(),
        to: address(OWNER).unwrap(),
        value: U256::from(50000000000000000u64),
    };
    let quote_out = Transfer {
        from: address(OWNER).unwrap(),
        to: address(VAULT).unwrap(),
        value: U256::from(40000000000000000u64),
    };
    let transfer = |token: &str, t: Transfer| json!({"address":token,"topics":t.encode_topics().iter().map(|v|v.0).collect::<Vec<_>>(),"data":Bytes::from(t.encode_data())});
    mock.state.lock().unwrap().receipt = json!({"transactionHash":hash,"status":"0x1","blockNumber":head.tag,"blockHash":head.hash,
        "logs":[transfer(&c.base_token,base_in),transfer(&c.quote_token,quote_out)]});
    assert!(
        !uni.settle(&mut recovered, address(OWNER).unwrap(), &head)
            .await
            .unwrap()
    );
    assert_eq!(recovered.state.pending.len(), 1);
    let mut confirmed = head.clone();
    confirmed.number += c.confirmations;
    assert!(
        uni.settle(&mut recovered, address(OWNER).unwrap(), &confirmed)
            .await
            .unwrap()
    );
    assert!(recovered.state.pending.is_empty());
    assert!(recovered.state.hedge_tx.is_none());
    assert_eq!(recovered.state.estimated_pnl_raw, "9000000000000000");
    assert_eq!(recovered.state.cumulative_loss_raw, "0");
    assert_eq!(
        mock.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|m| m.as_str() == "eth_sendRawTransaction")
            .count(),
        1
    );
}
