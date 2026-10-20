use clap::Parser;
use nevdex_arbitrage::{
    config::Config,
    engine::{Engine, Options},
};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(about = "NevDEX x Uniswap V3 arbitrage. Default: continuous read-only monitoring.")]
struct Args {
    #[arg(long, default_value = "arbitrage.example.json")]
    config: PathBuf,
    /// Validate market and deployment, then exit (read-only, 30s deadline).
    #[arg(long,conflicts_with_all=["once","live"])]
    check: bool,
    /// Preview one fresh order book and executable quote pair (read-only, 30s deadline).
    #[arg(long, conflicts_with = "live")]
    once: bool,
    /// Enable real orders, cancellations, and hedge transactions.
    #[arg(long)]
    live: bool,
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let stop = CancellationToken::new();
    let signal_stop = stop.clone();
    let signals = tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("Cannot register SIGTERM");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal_stop.cancel();
    });
    let result = async {
        let config = Config::load(&args.config, args.live)?;
        Engine::new(config)?
            .run(
                Options {
                    check: args.check,
                    once: args.once,
                    live: args.live,
                },
                stop,
            )
            .await
    }
    .await;
    signals.abort();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
