use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39333);
    let db_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".clawdorio")
        .join("clawdorio.db");

    eprintln!("[clawdorio] server listening on http://{addr}");
    clawdorio_server::serve(addr, db_path).await
}

