//! `rspace-registry` — OCI Distribution Spec v1.1 registry head.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rspace_registry::{build_router, AppState};
use rspace_registry_core::gc;
use rspace_registry_fs::FsStorage;

#[derive(Parser, Debug)]
#[command(
    name = "rspace-registry",
    version,
    about = "Rust OCI Distribution Spec v1.1 registry head"
)]
struct Cli {
    /// Address to listen on, e.g. `0.0.0.0:5000`.
    #[arg(long, default_value = "0.0.0.0:5000", global = true)]
    listen: String,

    /// Data directory for blobs and manifests (filesystem backend).
    #[arg(long, default_value = "/var/lib/rspace_registry", global = true)]
    data: PathBuf,

    /// Path to an htpasswd file. Without one the registry runs without
    /// auth — DO NOT do this in production.
    #[arg(long, global = true)]
    auth_file: Option<PathBuf>,

    /// Realm to advertise in the `WWW-Authenticate` challenge.
    #[arg(long, default_value = "rspace-registry", global = true)]
    realm: String,

    /// TLS certificate (PEM). Provide together with `--key` to enable
    /// HTTPS. Mandatory in production.
    #[arg(long, global = true)]
    cert: Option<PathBuf>,
    #[arg(long, global = true)]
    key: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the HTTP service (default if no subcommand given).
    Serve,
    /// One-shot mark-and-sweep GC over the data directory, then exit.
    Gc,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rspace_registry=info,axum=info,tower_http=info".into()),
        )
        .init();

    let mut cli = Cli::parse();
    let cmd = cli.cmd.take().unwrap_or(Command::Serve);

    let storage = Arc::new(
        FsStorage::new(&cli.data)
            .with_context(|| format!("opening data dir {}", cli.data.display()))?,
    );

    match cmd {
        Command::Serve => serve(&cli, storage).await,
        Command::Gc => {
            let report = gc::run(&*storage).await?;
            println!(
                "gc: scanned {} repos / {} manifests, {} reachable blobs, deleted {} blobs ({} bytes)",
                report.repos_scanned,
                report.manifests_scanned,
                report.reachable_blobs,
                report.deleted_blobs,
                report.deleted_bytes
            );
            Ok(())
        }
    }
}

async fn serve(cli: &Cli, storage: Arc<FsStorage>) -> Result<()> {
    let mut state = AppState::new(storage);
    state.realm = cli.realm.clone();

    match &cli.auth_file {
        Some(p) => {
            let h = rspace_registry::auth::Htpasswd::load(p)
                .with_context(|| format!("loading htpasswd from {}", p.display()))?;
            state.auth = Some(Arc::new(h));
            tracing::info!(file = %p.display(), "auth enabled (htpasswd)");
        }
        None => {
            tracing::warn!(
                "no --auth-file set; registry is unauthenticated. NEVER do this in production."
            );
        }
    }

    let addr: SocketAddr = cli
        .listen
        .parse()
        .with_context(|| format!("parsing --listen {}", cli.listen))?;

    tracing::info!(
        listen = %addr,
        data = %cli.data.display(),
        tls = cli.cert.is_some(),
        "rspace-registry starting"
    );

    let app = build_router(state);

    match (&cli.cert, &cli.key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| {
                    format!("loading TLS from cert={} key={}", cert.display(), key.display())
                })?;
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
        _ => anyhow::bail!("--cert and --key must be provided together"),
    }
    Ok(())
}
