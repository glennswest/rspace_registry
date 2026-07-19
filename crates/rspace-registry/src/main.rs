//! `rspace-registry` — OCI Distribution Spec v1.1 registry head.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use rspace_registry::k8s::{ApiReviewer, K8sAuth, K8sAuthConfig};
use rspace_registry::router::RepoClass;
use rspace_registry::{build_router, AppState, Auth};
use rspace_registry_core::migrate;
use rspace_registry_core::{
    gc, replicate, MultiStore, Partition, Quota, QuotaStorage, ReplicateConfig, RepoRouter,
    RouteRule, Storage,
};
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

    /// Per-repo storage placement: `pattern=/path` repeatable. Patterns
    /// support `*` (any run) and `?` (one char); longest match wins.
    /// Example:
    ///   --repo-root 4.18.2/kernel=/mnt/fast/418-kernel
    ///   --repo-root 4.18.2/*=/mnt/slow/418
    ///   --repo-root *=/mnt/fast/default
    /// When given, takes priority over `--data` / `--partition` as the
    /// top-level Storage; a catchall (`*=`) rule is required if you
    /// want unmapped repos to land somewhere.
    #[arg(long = "repo-root", value_name = "pattern=/path", global = true)]
    repo_roots: Vec<String>,

    /// Named repo class: `name=/path` repeatable. Readable sugar for a
    /// `--repo-root name/*=/path` rule — declare `system`, `partner`,
    /// `customer`, `microvm`, `data`, … each on its own volume. Composes
    /// with `--repo-root` (longest-match still wins, so a more specific
    /// `--repo-root name/keep=/x` overrides the class). Example:
    ///   --repo-class system=/mnt/system
    ///   --repo-class microvm=/mnt/nvme
    ///   --repo-class data=/mnt/bulk
    #[arg(long = "repo-class", value_name = "name=/path", global = true)]
    repo_classes: Vec<String>,

    /// Per-class storage quota: `pattern=<size>` repeatable, longest-match
    /// wins. Size accepts a byte count or a binary-unit suffix
    /// (`K`/`Ki`, `M`/`Mi`, `G`/`Gi`, `T`/`Ti`). Caps blob bytes on the
    /// class's volume; over-quota pushes get `413`. Example:
    ///   --quota 'data/*=500Gi'
    ///   --quota 'customer/*=2Ti'
    /// Only meaningful with a repo-routed layout (`--repo-root`/`--repo-class`).
    #[arg(long = "quota", value_name = "pattern=size", global = true)]
    quotas: Vec<String>,

    /// Quota by class name: `name=<size>` — sugar for `--quota name/*=<size>`.
    #[arg(long = "quota-class", value_name = "name=size", global = true)]
    quota_classes: Vec<String>,

    /// Usage-cache TTL for quota accounting (e.g. `30s`). Lower is more
    /// accurate under bursts but rescans the volume more often.
    #[arg(long = "quota-cache-ttl", default_value = "30s", global = true)]
    quota_cache_ttl: String,

    /// Auth mode: `none` (default), `htpasswd`, or `k8s`. `htpasswd` is
    /// implied when `--auth-file` is given. `k8s` delegates authn/authz to
    /// the cluster (TokenReview + SubjectAccessReview).
    #[arg(long, global = true)]
    auth: Option<String>,

    /// Path to an htpasswd file. Without one (and without `--auth k8s`) the
    /// registry runs without auth — DO NOT do this in production.
    #[arg(long, global = true)]
    auth_file: Option<PathBuf>,

    /// `--auth k8s`: API server URL. Defaults to the in-cluster
    /// `KUBERNETES_SERVICE_HOST`/`_PORT` env.
    #[arg(long = "auth-k8s-api", value_name = "url", global = true)]
    auth_k8s_api: Option<String>,

    /// `--auth k8s`: SAR resource as `group/resource`. Default
    /// `rspace.io/repositories`.
    #[arg(
        long = "auth-k8s-resource",
        value_name = "group/res",
        default_value = "rspace.io/repositories",
        global = true
    )]
    auth_k8s_resource: String,

    /// `--auth k8s`: namespace to authorize single-segment repos and the
    /// catalog against. Unset rejects such requests.
    #[arg(long = "auth-k8s-default-ns", value_name = "ns", global = true)]
    auth_k8s_default_ns: Option<String>,

    /// `--auth k8s`: TokenReview/SAR verdict cache TTL (e.g. `2m`).
    #[arg(long = "auth-cache-ttl", default_value = "2m", global = true)]
    auth_cache_ttl: String,

    /// `--auth k8s`: absolute URL of this registry's token-exchange
    /// endpoint, advertised as `realm=` in the Bearer challenge. Defaults
    /// to `http(s)://<listen>/token` (https when `--cert` is set). Set this
    /// to the externally-reachable URL in production, since `--listen` may
    /// bind `0.0.0.0`.
    #[arg(long = "auth-k8s-token-url", value_name = "url", global = true)]
    auth_k8s_token_url: Option<String>,

    /// `--auth k8s`: skip auth for loopback (`127.0.0.1`/`::1`) clients — the
    /// boot-order fast path so a node can serve preloaded images before the
    /// API server exists.
    #[arg(long = "auth-allow-loopback", global = true)]
    auth_allow_loopback: bool,

    /// Realm to advertise in the `WWW-Authenticate` challenge (htpasswd mode)
    /// or as the `Bearer` token realm (k8s mode).
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
    /// Offline one-shot class/pattern migration between volumes, then
    /// exit. Build the CURRENT layout with `--repo-root`/`--repo-class`,
    /// give the destination with `--to`; after it completes, update that
    /// flag to point the class at `--to` for subsequent runs. Requires a
    /// repo-routed layout.
    Migrate {
        /// Exact route pattern to migrate (e.g. `data/*`). Mutually
        /// exclusive with `--class`.
        #[arg(long)]
        pattern: Option<String>,
        /// Named class to migrate; expands to `<class>/*`.
        #[arg(long)]
        class: Option<String>,
        /// Destination volume path.
        #[arg(long)]
        to: PathBuf,
        /// Delete + GC the old volume after cutover.
        #[arg(long)]
        drain: bool,
    },
}

/// Result of resolving the CLI's storage flags.
///
/// Precedence: `--repo-root` (one or more) > `--partition` (one or more) >
/// `--data` (single root). `multi`/`router` are exposed when present so
/// admin endpoints can introspect or repoint them.
struct StorageSetup {
    storage: Arc<dyn Storage>,
    multi: Option<Arc<MultiStore>>,
    router: Option<Arc<RepoRouter>>,
    quota: Option<Arc<QuotaStorage>>,
    classes: Vec<RepoClass>,
}

fn build_storage(cli: &Cli) -> Result<StorageSetup> {
    // ---- Repo-routed mode (highest precedence) -------------------------
    // Both `--repo-root pattern=/path` and `--repo-class name=/path` (sugar
    // for `name/*=/path`) contribute route rules to one RepoRouter.
    if !cli.repo_roots.is_empty() || !cli.repo_classes.is_empty() {
        let mut rules = Vec::with_capacity(cli.repo_roots.len() + cli.repo_classes.len());
        let mut classes = Vec::with_capacity(cli.repo_classes.len());

        for raw in &cli.repo_classes {
            let (name, path) = raw
                .split_once('=')
                .ok_or_else(|| anyhow!("--repo-class {raw:?} must be name=/path"))?;
            if name.is_empty() {
                return Err(anyhow!("--repo-class {raw:?} has empty name"));
            }
            if name.contains('/') || name.contains('*') {
                return Err(anyhow!(
                    "--repo-class name {name:?} must be a bare class name (no '/' or '*')"
                ));
            }
            let pattern = format!("{name}/*");
            let backend = Arc::new(
                FsStorage::new(path)
                    .with_context(|| format!("opening repo class {name}={path}"))?,
            ) as Arc<dyn Storage>;
            rules.push(RouteRule {
                pattern: pattern.clone(),
                backend,
            });
            classes.push(RepoClass {
                name: name.to_string(),
                pattern,
                root: path.to_string(),
            });
        }

        for raw in &cli.repo_roots {
            let (pattern, path) = raw
                .split_once('=')
                .ok_or_else(|| anyhow!("--repo-root {raw:?} must be pattern=/path"))?;
            if pattern.is_empty() {
                return Err(anyhow!("--repo-root {raw:?} has empty pattern"));
            }
            let backend = Arc::new(
                FsStorage::new(path)
                    .with_context(|| format!("opening repo root {pattern}={path}"))?,
            ) as Arc<dyn Storage>;
            rules.push(RouteRule {
                pattern: pattern.to_string(),
                backend,
            });
        }
        let router = Arc::new(RepoRouter::new(rules)?);

        // Optional per-class quotas wrap the router as the top-level storage.
        let quotas = parse_quotas(&cli.quotas, &cli.quota_classes)?;
        let (storage, quota): (Arc<dyn Storage>, Option<Arc<QuotaStorage>>) = if quotas.is_empty() {
            (router.clone() as Arc<dyn Storage>, None)
        } else {
            let ttl = parse_duration(&cli.quota_cache_ttl)?;
            let qs = Arc::new(QuotaStorage::new(router.clone(), quotas, ttl));
            (qs.clone() as Arc<dyn Storage>, Some(qs))
        };

        return Ok(StorageSetup {
            storage,
            multi: None,
            router: Some(router),
            quota,
            classes,
        });
    }

    // ---- Multi-partition mode -----------------------------------------
    if !cli.partitions.is_empty() {
        let mut parsed = Vec::with_capacity(cli.partitions.len());
        for raw in &cli.partitions {
            let (name, path) = raw
                .split_once('=')
                .ok_or_else(|| anyhow!("--partition {raw:?} must be name=/path"))?;
            if name.is_empty() {
                return Err(anyhow!("--partition {raw:?} has empty name"));
            }
            let storage = Arc::new(
                FsStorage::new(path).with_context(|| format!("opening partition {name}={path}"))?,
            ) as Arc<dyn Storage>;
            parsed.push(Partition {
                name: name.to_string(),
                storage,
            });
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
        return Ok(StorageSetup {
            storage: multi.clone() as Arc<dyn Storage>,
            multi: Some(multi),
            router: None,
            quota: None,
            classes: Vec::new(),
        });
    }

    // ---- Default: single-root FsStorage --------------------------------
    let s = Arc::new(
        FsStorage::new(&cli.data)
            .with_context(|| format!("opening data dir {}", cli.data.display()))?,
    ) as Arc<dyn Storage>;
    Ok(StorageSetup {
        storage: s,
        multi: None,
        router: None,
        quota: None,
        classes: Vec::new(),
    })
}

/// Parse a byte size: a plain count, or a binary-unit suffix
/// (`K`/`Ki`, `M`/`Mi`, `G`/`Gi`, `T`/`Ti` — all powers of 1024).
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().with_context(|| format!("invalid size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "ki" => 1 << 10,
        "m" | "mi" => 1 << 20,
        "g" | "gi" => 1 << 30,
        "t" | "ti" => 1 << 40,
        other => return Err(anyhow!("unknown size unit {other:?} in {s:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| anyhow!("size {s:?} overflows u64"))
}

/// Build the quota list from `--quota pattern=size` and `--quota-class
/// name=size` (the latter expands to `name/*=size`).
fn parse_quotas(quotas: &[String], quota_classes: &[String]) -> Result<Vec<Quota>> {
    let mut out = Vec::with_capacity(quotas.len() + quota_classes.len());
    for raw in quota_classes {
        let (name, size) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--quota-class {raw:?} must be name=size"))?;
        if name.is_empty() {
            return Err(anyhow!("--quota-class {raw:?} has empty name"));
        }
        out.push(Quota {
            pattern: format!("{name}/*"),
            max_bytes: parse_size(size)?,
        });
    }
    for raw in quotas {
        let (pattern, size) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--quota {raw:?} must be pattern=size"))?;
        if pattern.is_empty() {
            return Err(anyhow!("--quota {raw:?} has empty pattern"));
        }
        out.push(Quota {
            pattern: pattern.to_string(),
            max_bytes: parse_size(size)?,
        });
    }
    Ok(out)
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
        Command::Migrate {
            pattern,
            class,
            to,
            drain,
        } => {
            let router = setup.router.ok_or_else(|| {
                anyhow!("`migrate` requires a repo-routed layout (--repo-root / --repo-class)")
            })?;
            let pattern = match (pattern, class) {
                (Some(p), _) if !p.is_empty() => p,
                (_, Some(c)) if !c.is_empty() => format!("{c}/*"),
                _ => return Err(anyhow!("`migrate` requires --pattern or --class")),
            };
            let new_backend = Arc::new(
                FsStorage::new(&to)
                    .with_context(|| format!("opening destination {}", to.display()))?,
            ) as Arc<dyn Storage>;
            let report = migrate::run(&router, &pattern, new_backend, drain).await?;
            println!(
                "migrate: pattern {pattern:?} -> {} | {} repos, {} blobs ({} bytes), {} manifests copied; purged {} blobs ({} bytes); cutover={} in {} ms",
                to.display(),
                report.repos_migrated,
                report.blobs_copied,
                report.bytes_copied,
                report.manifests_copied,
                report.blobs_purged,
                report.bytes_purged,
                report.cutover,
                report.duration_ms,
            );
            if drain {
                println!(
                    "note: bytes moved and old volume drained. Update your --repo-{} flag to point {pattern:?} at {} for the next run.",
                    if pattern.ends_with("/*") { "class/--repo-root" } else { "root" },
                    to.display(),
                );
            }
            Ok(())
        }
    }
}

/// Resolve the auth flags into an `Auth` mode (or `None` for no auth).
///
/// Precedence: an explicit `--auth <mode>` wins; otherwise `--auth-file`
/// implies htpasswd; otherwise no auth (with a loud warning).
fn build_auth(cli: &Cli) -> Result<Option<Auth>> {
    let mode = cli.auth.as_deref().unwrap_or_else(|| {
        if cli.auth_file.is_some() {
            "htpasswd"
        } else {
            "none"
        }
    });

    match mode {
        "none" => {
            tracing::warn!(
                "no auth configured; registry is unauthenticated. NEVER do this in production."
            );
            Ok(None)
        }
        "htpasswd" => {
            let path = cli
                .auth_file
                .as_ref()
                .ok_or_else(|| anyhow!("--auth htpasswd requires --auth-file"))?;
            let h = rspace_registry::auth::Htpasswd::load(path)
                .with_context(|| format!("loading htpasswd from {}", path.display()))?;
            tracing::info!(file = %path.display(), "auth enabled (htpasswd)");
            Ok(Some(Auth::Htpasswd(Arc::new(h))))
        }
        "k8s" => {
            let (group, resource) = cli
                .auth_k8s_resource
                .split_once('/')
                .ok_or_else(|| anyhow!("--auth-k8s-resource must be group/resource"))?;
            let cache_ttl = parse_duration(&cli.auth_cache_ttl)?;
            let reviewer = ApiReviewer::in_cluster(cli.auth_k8s_api.as_deref())
                .context("initialising Kubernetes API reviewer for --auth k8s")?;
            let token_realm = cli.auth_k8s_token_url.clone().unwrap_or_else(|| {
                let scheme = if cli.cert.is_some() { "https" } else { "http" };
                format!("{scheme}://{}/token", cli.listen)
            });
            let cfg = K8sAuthConfig {
                resource_group: group.to_string(),
                resource: resource.to_string(),
                default_namespace: cli.auth_k8s_default_ns.clone(),
                cache_ttl,
                allow_loopback: cli.auth_allow_loopback,
                token_realm,
                service: "rspace-registry".to_string(),
            };
            tracing::info!(
                resource = %cli.auth_k8s_resource,
                default_ns = ?cli.auth_k8s_default_ns,
                allow_loopback = cli.auth_allow_loopback,
                cache_ttl_secs = cache_ttl.as_secs(),
                "auth enabled (k8s: TokenReview + SubjectAccessReview)"
            );
            Ok(Some(Auth::K8s(Arc::new(K8sAuth::new(
                cfg,
                Box::new(reviewer),
            )))))
        }
        other => Err(anyhow!(
            "unknown --auth mode {other:?} (expected none|htpasswd|k8s)"
        )),
    }
}

async fn serve(cli: &Cli, setup: StorageSetup) -> Result<()> {
    let mut state = AppState::new(setup.storage.clone());
    if let Some(m) = &setup.multi {
        state = state.with_multi(m.clone());
    }
    if let Some(r) = &setup.router {
        state = state.with_router(r.clone());
    }
    if !setup.classes.is_empty() {
        state = state.with_classes(setup.classes.clone());
    }
    if let Some(q) = &setup.quota {
        state = state.with_quota(q.clone());
    }
    state.realm = cli.realm.clone();
    state.auth = build_auth(cli)?;

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
                    format!(
                        "loading TLS from cert={} key={}",
                        cert.display(),
                        key.display()
                    )
                })?;
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(anyhow::Error::from)
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .map_err(anyhow::Error::from)
        }
        _ => Err(anyhow!("--cert and --key must be provided together")),
    };

    if let Some(h) = replicate_handle {
        h.abort();
    }
    serve_result
}
