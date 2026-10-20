use alloy::{
    primitives::{
        Bytes, U256,
        aliases::{U24, U160},
    },
    signers::local::PrivateKeySigner,
    sol_types::{SolCall, SolEvent},
};
use nevdex_arbitrage::{
    abi::*,
    config::Config,
    market::{Depth, Market},
    numbers::*,
    orders::*,
    planner::{CrossVenue, Quote, Side, Strategy, StrategyInput},
    state::{Fill, HedgeTx, Intent, Journal},
    transport::Rpc,
    uniswap::{ChainLog, Uniswap},
};
use serde_json::{Value, json};

fn config() -> Config {
    serde_json::from_str(include_str!("../arbitrage.example.json")).unwrap()
}
fn market(c: &Config) -> Market {
    Market {
        id: c.market.clone(),
        is_published: true,
        base_token_address: c.base_token.clone(),
        quote_token_address: c.quote_token.clone(),
        base_token_decimals: 18,
        quote_token_decimals: 18,
        price_decimals: 4,
        amount_decimals: 4,
        min_order_size: "0".into(),
        maker_fee_rate: "0.001".into(),
        taker_fee_rate: "0.002".into(),
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/order.json")).unwrap()
}
fn built() -> BuiltOrder {
    serde_json::from_value(fixture()["order"].clone()).unwrap()
}
fn quote() -> Quote {
    Quote {
        side: Side::Sell,
        price: "0.5".into(),
        amount: "0.1".into(),
    }
}
fn intent() -> Intent {
    Intent {
        id: built().id,
        relayer: built().json.relayer,
        quote: quote(),
        expires: 1700000060,
        created_at: 1700000000000,
        seen: true,
        last_cancel_at: 0,
        filled_base_raw: "100000000000000000".into(),
    }
}

#[test]
fn exact_units_and_uint256_limits() {
    assert_eq!(
        units("9007199254740993.000000000000000001", 18)
            .unwrap()
            .to_string(),
        "9007199254740993000000000000000001"
    );
    assert!(units("0.0000001", 6).is_err());
    assert_eq!(units(&max_u256().to_string(), 0).unwrap(), max_u256());
    assert!(units(&(max_u256() + 1u32).to_string(), 0).is_err());
    for text in ["-1", "1e18", "NaN", ".1", "1."] {
        assert!(decimal(text).is_err());
    }
    assert_eq!(display(&raw("-1000100").unwrap(), 6), "-1.0001");
    assert_eq!(slip(&1.into(), 50, true), 2.into());
    assert_eq!(slip(&1.into(), 50, false), 0.into());
}
#[test]
fn config_rejects_unsafe_types_fields_and_endpoints() {
    let mut v = serde_json::to_value(config()).unwrap();
    v["orderAmount"] = json!(0.1);
    assert!(serde_json::from_value::<Config>(v).is_err());
    let mut c = config();
    c.endpoint = "https://example.com/?key=secret".into();
    assert!(c.validate(false).is_err());
    c.endpoint = "http://example.com/api/".into();
    assert!(c.validate(false).is_err());
    c.endpoint = "http://127.0.0.1:1234/api/".into();
    assert!(c.validate(false).is_ok());
    assert!(c.validate(true).is_err());
    let mut v = serde_json::to_value(c).unwrap();
    v["unknown"] = json!(true);
    assert!(serde_json::from_value::<Config>(v).is_err());
}
#[test]
fn api_numbers_preserve_precision_and_market_validates_fees() {
    let c = config();
    let mut m = market(&c);
    m.validate(&c).unwrap();
    m.maker_fee_rate = "0.000001".into();
    assert!(m.validate(&c).is_err());
    let order = r#"{"Id":"0x35d2c9d5235792ae23350c5a63f4d50053a0e9a53770ca30dad4c6d9274b3da1","TraderAddress":"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf","MarketId":"CASHCAT-USDG","Side":"sell","Type":"limit","Status":"pending","Price":9007199254740993.000000000000000001,"Amount":1,"AvailableAmount":0,"PendingAmount":1}"#;
    let o: OpenOrder = serde_json::from_str(order).unwrap();
    assert_eq!(o.price, "9007199254740993.000000000000000001");
    validate_open(&[o], &c, &built().json.trader).unwrap();
}
#[test]
fn planner_uses_worst_limit_and_full_size_depth() {
    let c = config();
    let m = market(&c);
    let mut depth: Depth = serde_json::from_value(
        json!({"sequence":1,"bids":[["1","0.05"],["0.1","1"]],"asks":[["2","1"]]}),
    )
    .unwrap();
    let sell = units("0.005", 18).unwrap();
    let buy = units("0.04", 18).unwrap();
    let evaluate = |d: &Depth| {
        CrossVenue
            .evaluate(StrategyInput {
                config: &c,
                market: &m,
                depth: d,
                uni_sell: &sell,
                uni_buy: &buy,
                created_at: 1,
            })
            .unwrap()
    };
    assert!(evaluate(&depth).is_none());
    depth.bids = vec![["1".into(), "0.1".into()]];
    let o = evaluate(&depth).unwrap();
    assert_eq!(o.quote.side, Side::Sell);
    assert!(o.net > 0.into());
    depth.bids[0][1] = "0.01".into();
    assert!(evaluate(&depth).is_none());
    depth.bids[0][0] = "3".into();
    assert!(depth.validate().is_err());
}
#[test]
fn node_order_hash_signature_and_router_abi_match() {
    let c = config();
    let m = market(&c);
    let o = built();
    assert_eq!(built_hash(&o).unwrap().to_string(), o.id);
    validate_built(&o, &quote(), &c, &m, &o.json.trader, 1700000000).unwrap();
    let signer: PrivateKeySigner = fixture()["testKey"].as_str().unwrap().parse().unwrap();
    assert_eq!(sign_order(&signer, &o.id).unwrap(), fixture()["signature"]);
    let encoded = Bytes::from(
        exactInputSingleCall {
            params: SwapParams {
                tokenIn: address(&c.base_token).unwrap(),
                tokenOut: address(&c.quote_token).unwrap(),
                fee: U24::from(3000),
                recipient: signer.address(),
                amount: U256::from(100000000000000000u64),
                bound: U256::from(49000000000000000u64),
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode(),
    );
    assert_eq!(encoded.to_string(), fixture()["swapInput"]);
}
#[test]
fn changed_signed_fields_are_rejected_even_with_recomputed_hash() {
    let c = config();
    let m = market(&c);
    for byte in [0, 1, 2, 9, 12, 22] {
        let mut o = built();
        let mut data: alloy::primitives::B256 = o.json.data.parse().unwrap();
        data[byte] ^= 1;
        o.json.data = data.to_string();
        o.id = built_hash(&o).unwrap().to_string();
        assert!(validate_built(&o, &quote(), &c, &m, &o.json.trader, 1700000000).is_err());
    }
    let mut o = built();
    o.json.quote_token_amount = "1".into();
    o.id = built_hash(&o).unwrap().to_string();
    assert!(validate_built(&o, &quote(), &c, &m, &o.json.trader, 1700000000).is_err());
    assert!(validate_built(&built(), &quote(), &c, &m, &built().json.trader, 1700000060).is_err());
}
fn uni() -> Uniswap {
    let c = config();
    Uniswap {
        market: market(&c),
        rpc: Rpc::new(&c).unwrap(),
        config: c,
        pool_fee: U24::from(3000),
    }
}
#[tokio::test]
async fn match_decodes_partial_fill_and_receipt_uses_actual_transfers() {
    let uni = uni();
    let owner = address(&built().json.trader).unwrap();
    let other = address("0x2222222222222222222222222222222222222222").unwrap();
    let event = Match {
        addressSet: AddressSet {
            baseToken: address(&uni.config.base_token).unwrap(),
            quoteToken: address(&uni.config.quote_token).unwrap(),
            relayer: address(&intent().relayer).unwrap(),
        },
        result: MatchResult {
            maker: owner,
            taker: other,
            buyer: other,
            makerFee: U256::from(2),
            makerRebate: U256::from(1),
            takerFee: U256::from(0),
            makerGasFee: U256::from(1),
            takerGasFee: U256::from(0),
            baseTokenFilledAmount: U256::from(50),
            quoteTokenFilledAmount: U256::from(100),
        },
    };
    let log = ChainLog {
        address: other,
        topics: vec![Match::SIGNATURE_HASH],
        data: Bytes::from(event.encode_data()),
        transaction_hash: built().id,
        log_index: "0x3".into(),
        block_number: "0x64".into(),
        block_hash: built().id,
        removed: false,
    };
    let fill = uni
        .decode_fill(&log, owner, Some(&intent()))
        .unwrap()
        .unwrap();
    assert_eq!(fill.base_raw, "50");
    assert_eq!(fill.quote_delta_raw, "98");
    assert_eq!(fill.hedge_side, Side::Buy);
    assert!(uni.decode_fill(&log, owner, None).is_err());
    let transfer = Transfer {
        from: owner,
        to: other,
        value: U256::from(50),
    };
    let receipt = json!({"logs":[{"address":uni.config.base_token,"topics":transfer.encode_topics().iter().map(|t| t.0).collect::<Vec<_>>(),"data":Bytes::from(transfer.encode_data())}]});
    assert_eq!(
        Uniswap::transfer_delta(&receipt, address(&uni.config.base_token).unwrap(), owner).unwrap(),
        (-50).into()
    );
    assert_eq!(
        Uniswap::transfer_delta(&receipt, address(&uni.config.quote_token).unwrap(), owner)
            .unwrap(),
        0.into()
    );
}
#[test]
#[cfg(unix)]
fn journal_locks_recovers_and_rejects_changed_identity_or_unsafe_files() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let temp = tempfile::tempdir().unwrap();
    let mut c = config();
    c.state_dir = temp.path().join("state");
    let m = market(&c);
    let owner = built().json.trader;
    assert!(Journal::open(&c, &m, "../invalid-wallet").is_err());
    assert!(!c.state_dir.exists());
    let mut j = Journal::open(&c, &m, &owner).unwrap();
    assert!(Journal::open(&c, &m, &owner).is_err());
    j.state.cursor = 100;
    j.state.cursor_hash = built().id;
    j.state.active = Some(intent());
    j.state.pending.push(Fill {
        id: format!("{}:3", built().id),
        hedge_side: Side::Buy,
        base_raw: "50".into(),
        quote_delta_raw: "98".into(),
        block: 100,
        block_hash: built().id,
    });
    j.state.hedge_tx = Some(HedgeTx {
        hash: built().id,
        nonce: 1,
        deadline: 1700000060,
        fill_id: j.state.pending[0].id.clone(),
        bound_raw: "100".into(),
        gas_budget_raw: "1".into(),
    });
    j.state.validate(&m).unwrap();
    j.save().unwrap();
    drop(j);
    let j = Journal::open(&c, &m, &owner).unwrap();
    assert_eq!(j.state.pending.len(), 1);
    assert_eq!(j.state.hedge_tx.as_ref().unwrap().nonce, 1);
    drop(j);
    let mut changed = c.clone();
    changed.confirmations += 1;
    assert!(Journal::open(&changed, &m, &owner).is_err());
    let file = c.state_dir.join(owner.to_lowercase()).join("state.json");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Journal::open(&c, &m, &owner).is_err());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&file, file.with_extension("saved")).unwrap();
    symlink(file.with_extension("saved"), &file).unwrap();
    assert!(Journal::open(&c, &m, &owner).is_err());
}
