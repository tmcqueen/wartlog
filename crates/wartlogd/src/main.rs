use clap::Parser;
use std::path::PathBuf;

/// WartLog daemon: collects logs and traces into the local store and serves the API.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// config file (default: $XDG_CONFIG_HOME/wartlog/wartlog.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// store directory (default: [storage] path, else $XDG_DATA_HOME/wartlog)
    #[arg(long)]
    store: Option<PathBuf>,
    /// API socket (default: $XDG_RUNTIME_DIR/wartlog/api.sock)
    #[arg(long)]
    socket: Option<PathBuf>,
    /// write to an external store (one created by a library writer), marking it as a daemon store
    #[arg(long)]
    adopt: bool,
    /// print the default config and exit
    #[arg(long)]
    print_default_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.print_default_config {
        print!("{}", wartlogd::config::default_text());
        return Ok(());
    }
    let filter = tracing_subscriber::EnvFilter::try_from_env("WARTLOG_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // under systemd the journal adds its own timestamps
    let under_systemd = std::env::var_os("INVOCATION_ID").is_some();
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if under_systemd {
        b.without_time().init();
    } else {
        b.init();
    }
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        let _ = tx.send(true);
    });
    let opts = wartlogd::RunOptions {
        config_path: args.config.unwrap_or_else(wartlogd::config::default_path),
        store: args.store,
        api_socket: args.socket,
        adopt: args.adopt,
        ..Default::default()
    };
    if let Err(e) = wartlogd::run(opts, rx).await {
        eprintln!("wartlogd: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}
