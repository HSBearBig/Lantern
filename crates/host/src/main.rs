use anyhow::{Context, Result};
use clap::Parser;
use lantern_host::{
    identity::{IdentityStore, Store},
    HostCommand, HostConfig, HostStatus,
};
use lantern_platform::CaptureOptions;
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};
use url::Url;

#[derive(Parser)]
#[command(about = "Lantern native stage-A host. Requires an unlocked logged-in desktop.")]
struct Args {
    #[arg(long, env = "LANTERN_SERVER", default_value = "http://localhost:8787")]
    server: Url,
    #[arg(long, default_value = "Lantern Desktop")]
    name: String,
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    pair: bool,
    #[arg(long)]
    list_pairs: bool,
    #[arg(long)]
    revoke: Option<String>,
    #[arg(long)]
    software: bool,
    #[arg(long)]
    allow_insecure_localhost: bool,
    #[arg(long, default_value_t = 1080)]
    max_height: u32,
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lantern_host=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let dir = args.data_dir.unwrap_or(
        directories::ProjectDirs::from("dev", "lantern", "Lantern")
            .context("Cannot determine user data directory")?
            .data_local_dir()
            .to_path_buf(),
    );
    if args.list_pairs || args.revoke.is_some() {
        let mut store = Store::open(dir)?;
        if let Some(id) = args.revoke {
            store.pairs.remove(&id);
            store.persist_pairs()?;
        }
        for (id, peer) in &store.pairs {
            println!("{id}\t{}", peer.name);
        }
        return Ok(());
    }
    let options = CaptureOptions {
        max_height: args.max_height,
        software: args.software,
        ..Default::default()
    };
    let config = HostConfig {
        server: args.server,
        name: args.name.clone(),
        data_dir: dir,
        options,
        pair_on_start: args.pair,
        allow_insecure_localhost: args.allow_insecure_localhost,
    };
    config.validate()?;
    let (tx, rx) = mpsc::channel(16);
    let (updates, mut status) = watch::channel(HostStatus::starting(args.name));
    let host = tokio::spawn(lantern_host::run(config, rx, updates));
    let mut previous_link = None;
    let printer = tokio::spawn(async move {
        while status.changed().await.is_ok() {
            let state = status.borrow_and_update().clone();
            if state.pairing_url != previous_link {
                if let Some(link) = &state.pairing_url {
                    // Deliberate local pairing output; never emitted to diagnostic logs.
                    println!("\nPairing link (single use, expires in 5 minutes):\n{link}\n");
                    let code = qrcode::QrCode::new(link.as_bytes());
                    if let Ok(code) = code {
                        println!(
                            "{}",
                            code.render::<qrcode::render::unicode::Dense1x2>()
                                .quiet_zone(true)
                                .build()
                        );
                    }
                }
                previous_link = state.pairing_url;
            }
        }
    });
    tokio::pin!(host);
    tokio::select! {
        result=&mut host=>result??,
        _=tokio::signal::ctrl_c()=>{let _=tx.send(HostCommand::Shutdown).await;host.await??;}
    }
    printer.abort();
    Ok(())
}
