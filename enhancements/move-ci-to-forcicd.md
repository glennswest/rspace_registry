# Enhancement: Move CI runs off GitHub Actions — forcicd is canonical

Authored 2026-05-24 from rspacefs side. The same change has already
landed in `rspacefs` (commit 46c779c, closes rspacefs#17). Mirroring the
ask here so rspace_registry follows the same pattern.

## Why

We're out of GitHub Actions bandwidth — confirmed by GitHub returning
`HTTP 403: API rate limit exceeded` on routine queries. forcicd.g8.lo
(Forgejo + act_runner) runs the same workflow YAML, on-LAN, with
prebuilt toolchain images and no public-runner queue. Duplicate runs
on GitHub are wasted minutes that we no longer have.

## What changes

Add a per-job gate to every job in `.github/workflows/*.yml`:

```yaml
jobs:
  fmt:
    if: github.server_url != 'https://github.com'
    runs-on: ubuntu-latest
    ...
```

GitHub Actions still records the workflow_run event but every job
evaluates the gate to false and skips immediately — **zero GHA
minutes consumed**. Forcicd's server_url is `http://forgejo:3000`,
so its jobs run normally.

If you ever need GitHub-side CI (external contributor without LAN
access, public PR verification), remove the gate or fire via
`workflow_dispatch` from the GitHub UI.

## Forcicd-specific gotchas (already learned in rspacefs)

- `Swatinem/rust-cache@v2` and `softprops/action-gh-release@v2` floating
  tags currently target node24; forcicd's act_runner v7 caps at node20.
  Pin to last-node20 versions:
  - `Swatinem/rust-cache@v2.7.3`
  - `softprops/action-gh-release@v2.0.8`

- `actions/upload-artifact@v4` hard-rejects non-github.com servers with
  `GHESNotSupportedError`. Gate that step too:
  ```yaml
  - name: upload artifacts
    if: github.server_url == 'https://github.com'
    uses: actions/upload-artifact@v4
    ...
  ```
  Or just drop it on forcicd — forgejo's artifact UX isn't useful for
  our workflow anyway.

## Releases

Keep `softprops/action-gh-release@v2.0.8` for tag-push events. On
forcicd, set repo secret `GH_PAT` to a fine-grained PAT scoped to
your github.com repo (Contents: read & write). The release step will
push tarballs to github.com/glennswest/rspace_registry/releases from
the forcicd build. Token resolution shape (from rspacefs's release.yml):

```yaml
token: ${{ github.server_url == 'https://github.com' && secrets.GITHUB_TOKEN || secrets.GH_PAT }}
```

## Acceptance

- [ ] All `ci.yml` jobs gated with `if: github.server_url != 'https://github.com'`
- [ ] `Swatinem/rust-cache` pinned to v2.7.3
- [ ] `actions/upload-artifact` step gated on github.com OR removed
- [ ] On forcicd, every push to main runs green
- [ ] On GitHub, the workflow run appears with every job `skipped`
- [ ] GH Actions minutes consumed per push: zero
