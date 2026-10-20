use alloy::primitives::{Address, B256, U256};
use anyhow::{Result, ensure};
use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::{One, Zero};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

pub fn decimal(s: &str) -> Result<BigRational> {
    ensure!(!s.is_empty() && s.len() <= 80, "Invalid decimal length");
    let parts: Vec<_> = s.split('.').collect();
    ensure!(
        parts.len() <= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "Invalid decimal"
    );
    let n = BigInt::parse_bytes(parts.concat().as_bytes(), 10).unwrap();
    Ok(BigRational::new(
        n,
        pow10(parts.get(1).map_or(0, |p| p.len() as u32)),
    ))
}
pub fn pow10(d: u32) -> BigInt {
    BigInt::from(10).pow(d)
}
pub fn raw(s: &str) -> Result<BigInt> {
    ensure!(!s.is_empty() && s.len() <= 160, "Invalid integer length");
    let digits = s.strip_prefix('-').unwrap_or(s);
    ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "Invalid integer"
    );
    Ok(BigInt::parse_bytes(s.as_bytes(), 10).unwrap())
}
pub fn unsigned(s: &str) -> Result<BigInt> {
    let n = raw(s)?;
    ensure!(n.sign() != Sign::Minus, "Negative unsigned amount");
    Ok(n)
}
pub fn units(s: &str, d: u32) -> Result<BigInt> {
    let n = decimal(s)? * BigRational::from_integer(pow10(d));
    ensure!(n.is_integer(), "Amount exceeds configured precision");
    let n = n.to_integer();
    uint(&n)?;
    Ok(n)
}
pub fn notional(price: &str, amount: &str, decimals: u32) -> Result<BigInt> {
    let n = (decimal(price)? * decimal(amount)? * BigRational::from_integer(pow10(decimals)))
        .to_integer();
    uint(&n)?;
    Ok(n)
}
pub fn uint(n: &BigInt) -> Result<U256> {
    let (sign, bytes) = n.to_bytes_be();
    ensure!(
        sign != Sign::Minus && bytes.len() <= 32,
        "Amount exceeds uint256"
    );
    Ok(U256::from_be_slice(&bytes))
}
pub fn bigint(n: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &n.to_be_bytes::<32>())
}
pub fn ceil_div(n: &BigInt, d: &BigInt) -> BigInt {
    (n + d - 1) / d
}
pub fn slip(n: &BigInt, bps: u32, buy: bool) -> BigInt {
    if buy {
        ceil_div(&(n * (10000 + bps)), &BigInt::from(10000))
    } else {
        n * (10000 - bps) / 10000
    }
}
pub fn display(n: &BigInt, d: u32) -> String {
    let sign = if n.sign() == Sign::Minus { "-" } else { "" };
    let mut digits = n.magnitude().to_str_radix(10);
    if d == 0 {
        return format!("{sign}{digits}");
    }
    while digits.len() <= d as usize {
        digits.insert(0, '0');
    }
    digits.insert(digits.len() - d as usize, '.');
    let digits = digits.trim_end_matches('0').trim_end_matches('.');
    format!("{sign}{digits}")
}
pub fn address(s: &str) -> Result<Address> {
    let a: Address = s.parse().map_err(|_| anyhow::anyhow!("Invalid address"))?;
    ensure!(!a.is_zero(), "Zero address");
    Ok(a)
}
pub fn hash(s: &str) -> Result<B256> {
    let h: B256 = s.parse().map_err(|_| anyhow::anyhow!("Invalid hash"))?;
    ensure!(!h.is_zero(), "Zero hash");
    Ok(h)
}
pub fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn hex_u64(s: &str) -> Result<u64> {
    ensure!(s.starts_with("0x"), "Invalid RPC quantity");
    u64::from_str_radix(&s[2..], 16).map_err(|_| anyhow::anyhow!("Invalid RPC quantity"))
}
pub fn wire_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(serde::de::Error::custom(
            "Expected decimal string or number",
        )),
    }
}
pub fn wire_u64<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    wire_string(d)?
        .parse()
        .map_err(|_| serde::de::Error::custom("Expected unsigned integer"))
}
pub fn log(message: &str, fields: Value) {
    println!(
        "{}",
        serde_json::json!({"timeUnixMs":now_ms(), "message":message, "fields":fields})
    );
}
pub fn positive(s: &str) -> Result<()> {
    ensure!(
        decimal(s)? > BigRational::zero(),
        "Expected positive decimal"
    );
    Ok(())
}
pub fn ratio_pass(net: &BigInt, cost: &BigInt, ratio: &str) -> Result<bool> {
    Ok(BigRational::from_integer(net.clone())
        >= BigRational::from_integer(cost.clone()) * decimal(ratio)?)
}
pub fn max_u256() -> BigInt {
    (BigInt::one() << 256) - 1
}
