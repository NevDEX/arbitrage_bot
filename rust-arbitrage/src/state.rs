//! Private recovery journal: exclusive OS lock, atomic replacement, file and directory fsync.
use crate::{
    config::Config,
    market::Market,
    numbers::*,
    planner::{Quote, Side},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub id: String,
    pub relayer: String,
    pub quote: Quote,
    pub expires: u64,
    pub created_at: u64,
    pub seen: bool,
    pub last_cancel_at: u64,
    pub filled_base_raw: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fill {
    pub id: String,
    pub hedge_side: Side,
    pub base_raw: String,
    pub quote_delta_raw: String,
    pub block: u64,
    pub block_hash: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HedgeTx {
    pub hash: String,
    pub nonce: u64,
    pub deadline: u64,
    pub fill_id: String,
    pub bound_raw: String,
    pub gas_budget_raw: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub identity: String,
    pub cursor: u64,
    pub cursor_hash: String,
    pub active: Option<Intent>,
    pub pending: Vec<Fill>,
    pub hedge_tx: Option<HedgeTx>,
    pub committed_quote_raw: String,
    pub cumulative_loss_raw: String,
    pub estimated_pnl_raw: String,
}
impl State {
    pub fn validate(&self, m: &Market) -> Result<()> {
        hash(&self.cursor_hash)?;
        unsigned(&self.committed_quote_raw)?;
        unsigned(&self.cumulative_loss_raw)?;
        raw(&self.estimated_pnl_raw)?;
        ensure!(
            self.pending.len() <= 100,
            "Recovery fill queue exceeds limit"
        );
        if let Some(o) = &self.active {
            hash(&o.id)?;
            address(&o.relayer)?;
            positive(&o.quote.price)?;
            positive(&o.quote.amount)?;
            units(&o.quote.price, m.price_decimals)?;
            units(&o.quote.amount, m.amount_decimals)?;
            ensure!(
                o.expires > 0
                    && o.created_at > 0
                    && unsigned(&o.filled_base_raw)?
                        <= units(&o.quote.amount, m.base_token_decimals)?,
                "Invalid persisted order intent"
            );
        }
        let mut total = num_bigint::BigInt::from(0);
        let mut seen = std::collections::HashSet::new();
        for f in &self.pending {
            let (tx, index) = f.id.split_once(':').context("Invalid fill identity")?;
            hash(tx)?;
            index.parse::<u64>().context("Invalid fill log index")?;
            hash(&f.block_hash)?;
            let o = self.active.as_ref().context("Fill without active order")?;
            let base = unsigned(&f.base_raw)?;
            ensure!(
                base > 0.into()
                    && f.block > 0
                    && f.block <= self.cursor
                    && f.hedge_side == o.quote.side.opposite()
                    && seen.insert(f.id.to_lowercase()),
                "Invalid persisted fill"
            );
            let delta = raw(&f.quote_delta_raw)?;
            ensure!(
                if f.hedge_side == Side::Sell {
                    delta < 0.into()
                } else {
                    delta > 0.into()
                },
                "Invalid fill quote direction"
            );
            total += base;
        }
        if let Some(o) = &self.active {
            ensure!(
                total <= unsigned(&o.filled_base_raw)?,
                "Pending quantity exceeds actual fills"
            );
        }
        if let Some(t) = &self.hedge_tx {
            hash(&t.hash)?;
            ensure!(
                self.pending.first().is_some_and(|f| f.id == t.fill_id)
                    && t.deadline > 0
                    && unsigned(&t.bound_raw)? > 0.into(),
                "Invalid hedge intent"
            );
            unsigned(&t.gas_budget_raw)?;
        }
        Ok(())
    }
}
pub struct Journal {
    pub state: State,
    file: PathBuf,
    _lock: File,
}

#[cfg(unix)]
fn private_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    options
}
#[cfg(not(unix))]
fn private_options() -> OpenOptions {
    OpenOptions::new()
}

#[cfg(unix)]
fn verify_private(meta: &fs::Metadata, dir: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        if dir { meta.is_dir() } else { meta.is_file() },
        "Private path has wrong file type"
    );
    // SAFETY: geteuid has no arguments and no memory access requirements.
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "Private paths must be owned by this user, mode 600 (files) or 700 (directories)"
    );
    Ok(())
}
#[cfg(not(unix))]
fn verify_private(_: &fs::Metadata, _: bool) -> Result<()> {
    anyhow::bail!("Live mode requires Unix private-file checks")
}

pub fn private_read(path: &Path, limit: u64) -> Result<Zeroizing<Vec<u8>>> {
    let mut file = private_options()
        .read(true)
        .open(path)
        .context("Cannot open private file")?;
    verify_private(&file.metadata()?, false)?;
    let mut bytes = Zeroizing::new(Vec::new());
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Private file exceeds limit");
    Ok(bytes)
}
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "State directory cannot be a symlink"
    );
    verify_private(&meta, true)
}
impl Journal {
    pub fn open(c: &Config, m: &Market, owner: &str) -> Result<Self> {
        address(owner)?;
        private_dir(&c.state_dir)?;
        let dir = c.state_dir.join(owner.to_lowercase());
        private_dir(&dir)?;
        let mut lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .open(dir.join("rust.lock"))?;
        verify_private(&lock.metadata()?, false)?;
        lock.try_lock_exclusive()
            .context("Wallet state is locked by another Rust process")?;
        lock.set_len(0)?;
        write!(
            lock,
            "{}",
            json!({"pid":std::process::id(),"startedAtUnixMs":now_ms()})
        )?;
        lock.sync_all()?;
        let identity = json!([
            "rust-arbitrage-v1",
            c.chain_id,
            c.endpoint,
            c.rpc_url,
            c.market,
            owner.to_lowercase(),
            c.vault_address.to_lowercase(),
            c.exchange_address.to_lowercase(),
            c.base_token.to_lowercase(),
            c.quote_token.to_lowercase(),
            c.uniswap_v3_pool.to_lowercase(),
            c.confirmations
        ])
        .to_string();
        let file = dir.join("state.json");
        let state = if fs::symlink_metadata(&file).is_ok() {
            let state: State = serde_json::from_slice(&private_read(&file, 1 << 20)?)
                .context("Invalid recovery state; retain the original file")?;
            ensure!(
                state.identity == identity,
                "Recovery identity mismatch; reconcile original deployment and wallet"
            );
            state.validate(m)?;
            state
        } else {
            ensure!(!file.try_exists()?, "Cannot inspect recovery state");
            State {
                identity,
                cursor: 0,
                cursor_hash: String::new(),
                active: None,
                pending: Vec::new(),
                hedge_tx: None,
                committed_quote_raw: "0".into(),
                cumulative_loss_raw: "0".into(),
                estimated_pnl_raw: "0".into(),
            }
        };
        Ok(Self {
            state,
            file,
            _lock: lock,
        })
    }
    pub fn save(&self) -> Result<()> {
        let dir = self.file.parent().unwrap();
        let temp = dir.join(format!(".state-{}-{}", std::process::id(), now_ms()));
        let result = (|| {
            let mut file = private_options().write(true).create_new(true).open(&temp)?;
            file.write_all(&serde_json::to_vec(&self.state)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.file)?;
            File::open(dir)?.sync_all()?;
            Ok(())
        })();
        if temp.exists() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}
