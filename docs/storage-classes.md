# Storage classes, quotas & migration

One registry instance can place different **classes** of repos on different
volumes, cap each class's consumption, and move a class to another volume
with no downtime. This exists because the classes have very different
shapes — boot-critical `system` base images (shared, read-mostly) shouldn't
share a volume with `data` (thousands of unique, non-dedupable volumes,
bursty) or `microvm` (one image → thousands of boots).

## Placing classes on volumes

`--repo-class <name>=<path>` puts every repo under `<name>/…` on its own
volume. It's readable sugar for `--repo-root <name>/*=<path>`:

```bash
rspace-registry serve \
  --repo-class system=/mnt/system \      # boot-critical, shared base
  --repo-class partner=/mnt/partner \
  --repo-class customer=/mnt/customer \  # multi-tenant; also the k8s-auth namespace boundary
  --repo-class microvm=/mnt/nvme \       # one image → 1000s of boots: hot reads
  --repo-class data=/mnt/bulk \          # 1000s of unique volumes, bursty, non-dedupable
  --repo-root '*=/mnt/default'           # catchall for everything else
```

Routing is longest-match, so a more specific `--repo-root
microvm/base=/mnt/xxl` overrides the `microvm` class for that one repo.
Blobs dedupe **within** a volume, not across — inherent to physical
per-class separation, and the point of it.

`GET /admin/repo-classes` lists the declared classes;
`GET /admin/repo-roots` shows the full routing ruleset;
`POST /admin/repo-root {pattern,root}` repoints a rule at runtime (routing
only — see migration below to move the bytes).

## Quotas

Cap a class's footprint on its volume so a bursty class can't starve a
boot-critical one:

```bash
  --quota-class data=500Gi \      # sugar for --quota data/*=500Gi
  --quota 'customer/*=2Ti'        # any glob; longest-match wins
```

Sizes accept a byte count or a binary suffix (`K`/`Ki`, `M`/`Mi`, `G`/`Gi`,
`T`/`Ti`). Over-quota pushes get **413 Payload Too Large**. Usage:

```bash
curl localhost:5000/admin/quotas
# { "quotas": [ { "pattern": "data/*", "max_bytes": 536870912000,
#                 "used_bytes": 421000000000, "used_pct": 78 } ] }
```

Accounting is **approximate** — a class's usage is its volume's blob bytes,
cached for `--quota-cache-ttl` (default `30s`) and nudged upward on each
accepted write. A small overshoot is possible under heavy concurrency (the
same trade-off Quay makes); lower the TTL to tighten it at the cost of more
volume scans. Idempotent re-pushes of an already-present blob add no bytes.

## Moving a class to another volume

Placement is a decision you can revisit *after* the fact. Migration copies
a class's bytes to a new volume with a **zero-miss cutover**: the route is
first overlaid (new volume as primary, old as read-fallback), then the
bytes are backfilled, then the route collapses onto the new volume — so
reads never miss, even mid-migration.

### Live (server running)

```bash
# Synchronous:
curl -X POST localhost:5000/admin/repo-migrate \
  -d '{"class":"data","to":"/mnt/bulk2","drain":true}'

# Background job (large classes): returns 202 + a job id to poll.
curl -X POST localhost:5000/admin/repo-migrate \
  -d '{"class":"data","to":"/mnt/bulk2","drain":true,"async":true}'
# → {"job_id":"…","state":"running"}
curl localhost:5000/admin/jobs/<id>   # running → done (+report) / failed (+error)
curl localhost:5000/admin/jobs        # list all jobs
```

`class:"data"` expands to pattern `data/*`; pass `pattern` directly for an
arbitrary glob. `drain:true` deletes the old volume's content and GCs it
after cutover to reclaim capacity. A more-specific rule (e.g. a pinned
`data/keep`) stays put — only repos that actually resolve to the source
volume move. Everything is content-addressed, so a migration is idempotent
and restartable; a failed backfill leaves the route on the correct overlay
(the union of both volumes), never losing reachability.

### Offline (server down / maintenance window)

```bash
rspace-registry migrate --class data --to /mnt/bulk2 --drain \
  --repo-class data=/mnt/bulk        # describe the CURRENT layout
# then point the flag at /mnt/bulk2 for subsequent server runs.
```

### In-flight uploads

An upload session started on the old volume before cutover is
backend-local; a later chunk routes to the new volume and the client
retries onto it. Finished blobs and manifests are unaffected. Avoid
migrating a class that's taking heavy fresh *uploads*.

## Admin endpoints

| Endpoint | Purpose |
|---|---|
| `GET /admin/repo-classes` | declared classes → volumes |
| `GET /admin/repo-roots` | full routing ruleset |
| `POST /admin/repo-root` | repoint a rule (routing only) |
| `POST /admin/repo-migrate` | move a class/pattern to a volume (`async`, `drain`) |
| `GET /admin/jobs[/<id>]` | background job status |
| `GET /admin/quotas` | per-class limit + usage |
| `POST /admin/gc` | mark-and-sweep GC |

Under `--auth k8s`, `/admin/*` requires `update` on the SAR resource; see
[k8s-auth.md](./k8s-auth.md).
