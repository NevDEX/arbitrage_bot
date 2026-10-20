//! A custom signal can reuse the same fill, hedge, and recovery implementation.
use anyhow::Result;
use nevdex_arbitrage::{
    config::Config,
    engine::{Engine, Options},
    numbers::units,
    planner::{CrossVenue, Opportunity, Strategy, StrategyInput},
};
use tokio_util::sync::CancellationToken;

struct MinimumNet;
impl Strategy for MinimumNet {
    fn evaluate(&self, input: StrategyInput<'_>) -> Result<Option<Opportunity>> {
        let minimum = units("0.01", input.market.quote_token_decimals)?;
        Ok(CrossVenue
            .evaluate(input)?
            .filter(|opportunity| opportunity.net >= minimum))
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let file = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "arbitrage.example.json".into());
    let config = Config::load(std::path::Path::new(&file), false)?;
    Engine::with_strategy(config, MinimumNet)?
        .run(
            Options {
                once: true,
                ..Options::default()
            },
            CancellationToken::new(),
        )
        .await
}
