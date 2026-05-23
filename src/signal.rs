use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(crate) fn install_handler() -> CancellationToken {
    let stop = CancellationToken::new();
    let signal = stop.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal.cancel();
    });
    stop
}

pub(crate) async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = wait_for_ctrl_c() => {}
            _ = wait_for_optional_signal(sigint.as_mut()) => {}
            _ = wait_for_optional_signal(sigterm.as_mut()) => {}
        }
    }
    #[cfg(not(unix))]
    {
        wait_for_ctrl_c().await;
    }
}

async fn wait_for_ctrl_c() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!("failed to register ctrl-c signal handler: {error}");
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn wait_for_optional_signal(signal: Option<&mut tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}
