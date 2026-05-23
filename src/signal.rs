use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn install_handler() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal.store(true, Ordering::SeqCst);
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
