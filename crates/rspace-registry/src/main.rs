//! `rspace-registry` — OCI Distribution Spec v1.1 registry head.
//!
//! This is the binary entry point. The HTTP routing and OCI handlers
//! live in `rspace-registry-core`; the default filesystem storage
//! backend is `rspace-registry-fs`. A future rspacefs-shared backend
//! will plug into the same `Storage` trait.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rspace-registry",
    version,
    about = "Rust OCI Distribution Spec v1.1 registry head"
)]
struct Cli {
    /// Address to listen on, e.g. `0.0.0.0:5000`.
    #[arg(long, default_value = "0.0.0.0:5000")]
    listen: String,

    /// Data directory for blobs and manifests (filesystem backend).
    #[arg(long, default_value = "/var/lib/rspace_registry")]
    data: PathBuf,

    /// Path to an htpasswd file. If unset, the registry runs without auth
    /// — DO NOT do this in production.
    #[arg(long)]
    auth_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rspace_registry=info,axum=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(
        listen = %cli.listen,
        data = %cli.data.display(),
        auth = cli.auth_file.is_some(),
        "rspace-registry starting"
    );
    if cli.auth_file.is_none() {
        tracing::warn!(
            "no --auth-file set; registry is unauthenticated. NEVER do this in production."
        );
    }

    // TODO: build Storage backend → router → listen
    eprintln!("TODO: implement HTTP service. See CLAUDE.md work plan.");
    Ok(())
}
