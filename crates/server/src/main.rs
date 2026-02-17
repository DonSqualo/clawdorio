use clap::Parser;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let addr = SocketAddr::new(args.host, args.port);
    let db_path = resolve_db_path(args.db)?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    eprintln!("[clawdorio] server listening on http://{actual}");

    let shutdown = async {
        // Best-effort shutdown on Ctrl+C (or SIGINT on unix).
        let _ = tokio::signal::ctrl_c().await;
    };
    let _ = clawdorio_server::serve_listener(listener, db_path, shutdown).await?;
    Ok(())
}

#[derive(Parser, Debug)]
#[command(name = "clawdorio-server")]
#[command(about = "Clawdorio headless API server", long_about = None)]
struct Args {
    /// Host/interface to bind (use 0.0.0.0 to expose on LAN/hosted env).
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,

    /// Port to bind (use 0 for an ephemeral port).
    #[arg(long, default_value_t = 39333)]
    port: u16,

    /// SQLite DB path. Defaults to $CLAWDORIO_DB or ~/.clawdorio/clawdorio.db
    #[arg(long)]
    db: Option<PathBuf>,
}

fn resolve_db_path(db: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = db {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("CLAWDORIO_DB") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".clawdorio")
        .join("clawdorio.db"))
}
