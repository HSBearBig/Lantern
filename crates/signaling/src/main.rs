use anyhow::{bail, Result};
use clap::Parser;
use std::net::SocketAddr;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "LANTERN_BIND", default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    #[arg(long, env = "LANTERN_ORIGIN", default_value = "http://localhost:8787")]
    origin: String,
    #[arg(long, env = "LANTERN_WEB_ROOT", default_value = "apps/controller/dist")]
    web_root: String,
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    if !args.origin.starts_with("https://")
        && !args.origin.starts_with("http://localhost:")
        && !args.origin.starts_with("http://127.0.0.1:")
    {
        bail!("Public origin must use HTTPS");
    }
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(bind = %args.bind, "signaling service listening");
    axum::serve(
        listener,
        lantern_signaling::router(args.origin, &args.web_root)
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
