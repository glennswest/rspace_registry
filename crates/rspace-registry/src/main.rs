//! `rspace-registry` — OCI Distribution Spec v1.1 registry head.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use rspace_registry::{build_router, AppState};
use rspace_registry_core::{gc, replicate, MultiStore, Partition, ReplicateConfig, Storage};
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

    /// Single-partition data directory (default backend). Ignored when
    /// `--partition` is given.
    #[arg(long, default_value = "/var/lib/rspace_registry", global = true)]
    data: PathBuf,

    /// Multi-partition mode: declare a partition as `name=/path`. May
    /// be passed multiple times. When set, `--primary` must name one
    /// of the partitions.
    #[arg(long = "partition", value_name = "name=/path", global = true)]
    partitions: Vec<String>,

    /// Name of the primary partition. Required when `--partition` is
    /// given more than once.
    #[arg(long, global = true)]
    primary: Option<String>,

    /// Reconciler interval (e.g. `60s`, `5m`). 0 disables the loop.
    #[arg(long, default_value = "60s", global = true)]
    replicate_interval: String,

    /// Optional shell-style tag glob restricting which manifests
    /// replicate (e.g. `prod-*`). Default: replicate everything.
    #[arg(long, global = true)]
    replicate_tag_glob: Option<String>,

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
    /// One-shot replication pass from primary to all secondaries,
    /// then exit. Requires `--partition` flags.
    Replicate,
}

/// Result of resolving the CLI's storage flags. `multi` is `Some` when
/// the operator declared more than one `--partition`; in that case the
/// `Storage` handed to the router is the MultiStore (also stored in
/// `multi` so admin endpoints can introspect it).
struct StorageSetup {
    storage: Arc<dyn Storage>,
    multi: Option<Arc<MultiStore>>,
}

fn build_storage(cli: &Cli) -> Result<StorageSetup> {
    if cli.partitions.is_empty() {
        let s = Arc::new(
            FsStorage::new(&cli.data)
                .with_context(|| format!("opening data dir {}", cli.data.display()))?,
        ) as Arc<dyn Storage>;
        return Ok(StorageSetup { storage: s, multi: None });
    }

    let mut parsed = Vec::with_capacity(cli.partitions.len());
    for raw in &cli.partitions {
        let (name, path) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--partition {raw:?} must be name=/path"))?;
        if name.is_empty() {
            return Err(anyhow!("--partition {raw:?} has empty name"));
        }
        let storage = Arc::new(
            FsStorage::new(path)
                .with_context(|| format!("opening partition {name}={path}"))?,
        ) as Arc<dyn Storage>;
        parsed.push(Partition { name: name.to_string(), storage });
    }

    let primary = match (&cli.primary, parsed.len()) {
        (Some(p), _) => p.clone(),
        (None, 1) => parsed[0].name.clone(),
        (None, _) => {
            return Err(anyhow!(
                "--primary required when more than one --partition is declared"
            ))
        }
    };

    let multi = Arc::new(MultiStore::new(parsed, &primary)?);
    Ok(StorageSetup {
        storage: multi.clone() as Arc<dyn Storage>,
        multi: Some(multi),
    })
}

/// Parse durations like `0`, `60s`, `5m`, `1h`. Empty units (e.g. `60`)
/// default to seconds.
fn parse_duration(s: &str) -> Result<Duration> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let s = s.trim();
    let (num, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| (&s[..i], &s[i..]))
        .unwrap_or((s, "s"));
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration {s:?}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        other => return Err(anyhow!("unknown duration unit {other:?} in {s:?}")),
    };
    Ok(Duration::from_secs(secs))
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
    let setup = build_storage(&cli)?;

    match cmd {
        Command::Serve => serve(&cli, setup).await,
        Command::Gc => {
            let report = gc::run(&*setup.storage).await?;
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
        Command::Replicate => {
            let multi = setup
                .multi
                .ok_or_else(|| anyhow!("`replicate` requires --partition flags"))?;
            let cfg = ReplicateConfig {
                tag_glob: cli.replicate_tag_glob.clone(),
            };
            let report = replicate::run(&multi, &cfg).await?;
            println!(
                "replicate: {} partitions, {} blobs ({} bytes), {} manifests in {} ms",
                report.partitions_scanned,
                report.blobs_copied,
                report.bytes_copied,
                report.manifests_copied,
                report.duration_ms
            );
            Ok(())
        }
    }
}

async fn serve(cli: &Cli, setup: StorageSetup) -> Result<()> {
    let mut state = AppState::new(setup.storage.clone());
    if let Some(m) = &setup.multi {
        state = state.with_multi(m.clone());
    }
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

    // Background replication loop — only meaningful with > 1 partition.
    let replicate_handle = if let Some(multi) = &setup.multi {
        if multi.partitions().len() > 1 {
            let interval = parse_duration(&cli.replicate_interval)?;
            if interval.is_zero() {
                tracing::info!("replicate: loop disabled (interval=0)");
                None
            } else {
                let cfg = ReplicateConfig {
                    tag_glob: cli.replicate_tag_glob.clone(),
                };
                tracing::info!(
                    interval_secs = interval.as_secs(),
                    glob = ?cli.replicate_tag_glob,
                    partitions = multi.partitions().len(),
                    primary = multi.primary().name,
                    "replicate: background reconciler started"
                );
                Some(replicate::spawn_loop(multi.clone(), cfg, interval))
            }
        } else {
            None
        }
    } else {
        None
    };

    tracing::info!(
        listen = %addr,
        tls = cli.cert.is_some(),
        multi = setup.multi.is_some(),
        "rspace-registry starting"
    );

    let app = build_router(state);

    let serve_result = match (&cli.cert, &cli.key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| {
                    format!("loading TLS from cert={} key={}", cert.display(), key.display())
                })?;
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await
                .map_err(anyhow::Error::from)
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await.map_err(anyhow::Error::from)
        }
        _ => Err(anyhow!("--cert and --key must be provided together")),
    };

    if let Some(h) = replicate_handle {
        h.abort();
    }
    serve_result
}
