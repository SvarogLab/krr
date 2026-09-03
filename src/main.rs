use anyhow::Result;
use clap::Parser;
use krr::cli::{Cli, Command};
use krr::runner::Runner;
use krr::strategy;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(cli.command, Command::Version) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let (config, strategy_args) = cli.command.into_settings()?;
    init_logging(&config);

    let strategy = strategy::find(&config.strategy, &strategy_args)?;
    let exit_code = Runner::new(config, strategy).run().await;

    // the report is written to stdout, so make sure it is out before exiting
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(exit_code);
}

/// Logs always go to stderr, so the report on stdout stays pipeable.
fn init_logging(config: &krr::config::Config) {
    // `--logtostderr` is accepted for compatibility with upstream command lines
    let level = match (config.quiet, config.verbose) {
        (true, _) => "krr=error",
        (_, true) => "krr=debug",
        _ => "krr=info",
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
