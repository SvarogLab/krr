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
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(exit_code);
}

/// Writes log lines to stderr, stepping around the progress bar while a scan runs.
struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        krr::runner::around_progress(|| std::io::stderr().write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
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
        .with_writer(|| LogWriter)
        .init();
}
