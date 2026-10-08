use std::io::IsTerminal;

use anyhow::{Context, Result};
use efema_server::Relay;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::cli::ServeArgs;

pub fn run(args: ServeArgs) -> Result<()> {
    // Logs go to stderr: a service manager collects both streams, and stdout
    // stays free for anything a script might want to read.
    // Colour only on a terminal: a service manager's journal or a log file
    // would otherwise keep the escape codes as text.
    let filter = EnvFilter::try_from_env("EFEMA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();

    let dir = args.data.dir()?;
    let relay = Relay::open(&dir).with_context(|| format!("cannot open the relay in {}", dir.display()))?;

    let runtime = tokio::runtime::Runtime::new().context("cannot start the async runtime")?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(args.listen)
            .await
            .with_context(|| format!("cannot listen on {}", args.listen))?;
        // The address actually bound: with port 0 the system picks one, and
        // this line is how anyone finds out which.
        let address = listener.local_addr().context("cannot read the address listened on")?;
        info!(
            "efema-server {} listening on http://{address}, data in {}",
            env!("CARGO_PKG_VERSION"),
            relay.data_dir().display()
        );
        relay.serve(listener, stopped()).await.context("the relay stopped serving")?;
        info!("stopped");
        Ok(())
    })
}

/// Completes on Ctrl+C, or on SIGTERM where there is one - what a service
/// manager sends to stop a daemon.
async fn stopped() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => return interrupt.await,
        };
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
    info!("stopping: finishing the requests in flight");
}
