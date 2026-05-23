use tokio_util::sync::CancellationToken;

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
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(stream) = sigint.as_mut() {
                    stream.recv().await;
                }
            } => {}
            _ = async {
                if let Some(stream) = sigterm.as_mut() {
                    stream.recv().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
