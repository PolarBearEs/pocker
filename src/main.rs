use std::io::IsTerminal;

use anstyle::AnsiColor;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use pocker::Cli;

#[tokio::main]
async fn main() {
    init_tracing();

    let cli = Cli::parse();

    if let Err(error) = pocker::run(cli).await {
        eprintln!("{} {error}", error_label());
        std::process::exit(1);
    }
}

fn init_tracing() {
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }
}

fn error_label() -> String {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        let style = AnsiColor::Red.on_default().bold();
        format!("{}error:{}", style.render(), style.render_reset())
    } else {
        "error:".to_string()
    }
}
