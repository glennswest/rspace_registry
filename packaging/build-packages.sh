#!/usr/bin/env bash
# Build distributable artifacts for rspace-registry:
#   * static (musl) release binary  — portable across distros
#   * .rpm   (Fedora/RHEL family)
#   * .deb   (Debian/Ubuntu family)
#   * OCI image archive             — FROM scratch (static binary)
#   * systemd unit                  — standalone copy (also bundled in rpm/deb)
#
# Designed to run on a Fedora/RHEL host with cargo, rpmbuild, dpkg-deb and
# buildah/podman available. Falls back to a glibc build (with a fedora-minimal
# OCI base) if the musl toolchain can't be set up.
#
# Usage: packaging/build-packages.sh [OUTDIR]   (default: ./dist)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
OUTDIR="$(cd "$(dirname "${1:-dist}")" 2>/dev/null && pwd || echo "$REPO_ROOT")/$(basename "${1:-dist}")"
mkdir -p "$OUTDIR"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
ARCH="$(uname -m)"                       # x86_64 / aarch64
case "$ARCH" in
  x86_64)  DEBARCH=amd64; MUSL_TARGET=x86_64-unknown-linux-musl ;;
  aarch64) DEBARCH=arm64; MUSL_TARGET=aarch64-unknown-linux-musl ;;
  *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
esac
echo ">> rspace-registry $VERSION  arch=$ARCH  out=$OUTDIR"

have() { command -v "$1" >/dev/null 2>&1; }
maybe_dnf() { have dnf && [ "$(id -u)" = 0 ] && dnf install -y "$@" >/dev/null 2>&1 || true; }

# ---- 1. Build the binary (musl static preferred) -----------------------------
STATIC=0
BIN=""
maybe_dnf musl-gcc cmake perl
if rustup target list --installed 2>/dev/null | grep -q "$MUSL_TARGET" \
   || rustup target add "$MUSL_TARGET" >/dev/null 2>&1; then
  export CC_x86_64_unknown_linux_musl=musl-gcc
  export CC_aarch64_unknown_linux_musl=musl-gcc
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
  if cargo build --release --target "$MUSL_TARGET" -p rspace-registry; then
    BIN="target/$MUSL_TARGET/release/rspace-registry"; STATIC=1
  fi
fi
if [ -z "$BIN" ]; then
  echo ">> musl build unavailable — falling back to native glibc build"
  cargo build --release -p rspace-registry
  BIN="target/release/rspace-registry"
fi
strip "$BIN" 2>/dev/null || true
echo ">> binary: $BIN (static=$STATIC)"
file "$BIN" || true

# Raw binary + systemd unit as standalone assets.
cp "$BIN" "$OUTDIR/rspace-registry-${VERSION}-${ARCH}"
cp packaging/rspace-registry.service "$OUTDIR/rspace-registry.service"

# ---- 2. RPM ------------------------------------------------------------------
RPMTOP="$(mktemp -d)"
mkdir -p "$RPMTOP"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}
cp "$BIN" "$RPMTOP/SOURCES/rspace-registry"
cp packaging/rspace-registry.service packaging/rspace-registry.env "$RPMTOP/SOURCES/"
cat > "$RPMTOP/SPECS/rspace-registry.spec" <<SPEC
%global debug_package %{nil}
Name:           rspace-registry
Version:        ${VERSION}
Release:        1%{?dist}
Summary:        OCI Distribution Spec v1.1 registry head (Rust)
License:        MIT
URL:            https://github.com/glennswest/rspace_registry
Source0:        rspace-registry
Source1:        rspace-registry.service
Source2:        rspace-registry.env
BuildArch:      ${ARCH}
BuildRequires:  systemd-rpm-macros
%{?systemd_requires}

%description
A Rust OCI Distribution Spec v1.1 registry head with per-repo storage
routing, per-class storage quotas, zero-downtime class migration, and
cluster-delegated (Kubernetes TokenReview/SubjectAccessReview) auth.
Ships as a self-contained static binary.

%install
install -D -m0755 %{SOURCE0} %{buildroot}%{_bindir}/rspace-registry
install -D -m0644 %{SOURCE1} %{buildroot}%{_unitdir}/rspace-registry.service
install -D -m0644 %{SOURCE2} %{buildroot}%{_sysconfdir}/rspace-registry/rspace-registry.env

%post
%systemd_post rspace-registry.service

%preun
%systemd_preun rspace-registry.service

%postun
%systemd_postun_with_restart rspace-registry.service

%files
%{_bindir}/rspace-registry
%{_unitdir}/rspace-registry.service
%config(noreplace) %{_sysconfdir}/rspace-registry/rspace-registry.env
SPEC
rpmbuild --define "_topdir $RPMTOP" -bb "$RPMTOP/SPECS/rspace-registry.spec"
cp "$RPMTOP"/RPMS/*/*.rpm "$OUTDIR/"
rm -rf "$RPMTOP"

# ---- 3. DEB ------------------------------------------------------------------
DEBROOT="$(mktemp -d)"
install -D -m0755 "$BIN" "$DEBROOT/usr/bin/rspace-registry"
install -D -m0644 packaging/rspace-registry.service "$DEBROOT/lib/systemd/system/rspace-registry.service"
install -D -m0644 packaging/rspace-registry.env "$DEBROOT/etc/rspace-registry/rspace-registry.env"
mkdir -p "$DEBROOT/DEBIAN"
cat > "$DEBROOT/DEBIAN/control" <<CTRL
Package: rspace-registry
Version: ${VERSION}
Architecture: ${DEBARCH}
Maintainer: Glenn West <glennswest@neuralcloudcomputing.com>
Section: admin
Priority: optional
Homepage: https://github.com/glennswest/rspace_registry
Description: OCI Distribution Spec v1.1 registry head (Rust)
 A Rust OCI Distribution registry head with per-repo storage routing,
 per-class storage quotas, zero-downtime class migration, and
 cluster-delegated (Kubernetes TokenReview/SAR) auth. Self-contained
 static binary; no runtime dependencies.
CTRL
echo "/etc/rspace-registry/rspace-registry.env" > "$DEBROOT/DEBIAN/conffiles"
cat > "$DEBROOT/DEBIAN/postinst" <<'PI'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  systemctl daemon-reload >/dev/null 2>&1 || true
  systemctl enable rspace-registry.service >/dev/null 2>&1 || true
fi
PI
cat > "$DEBROOT/DEBIAN/prerm" <<'PR'
#!/bin/sh
set -e
if [ "$1" = remove ]; then
  systemctl --no-reload disable rspace-registry.service >/dev/null 2>&1 || true
  systemctl stop rspace-registry.service >/dev/null 2>&1 || true
fi
PR
cat > "$DEBROOT/DEBIAN/postrm" <<'PO'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
  systemctl daemon-reload >/dev/null 2>&1 || true
fi
PO
chmod 0755 "$DEBROOT/DEBIAN/postinst" "$DEBROOT/DEBIAN/prerm" "$DEBROOT/DEBIAN/postrm"
dpkg-deb --root-owner-group --build "$DEBROOT" \
  "$OUTDIR/rspace-registry_${VERSION}_${DEBARCH}.deb"
rm -rf "$DEBROOT"

# ---- 4. OCI image archive ----------------------------------------------------
if [ "$STATIC" = 1 ]; then BASE="scratch"; else BASE="registry.fedoraproject.org/fedora-minimal:latest"; fi
IMG="rspace-registry:${VERSION}"
ctr="$(buildah from "$BASE")"
buildah copy "$ctr" "$BIN" /usr/local/bin/rspace-registry >/dev/null
buildah config \
  --port 5000 \
  --volume /var/lib/rspace_registry \
  --entrypoint '["/usr/local/bin/rspace-registry"]' \
  --cmd '["serve","--listen","0.0.0.0:5000","--data","/var/lib/rspace_registry"]' \
  --label "org.opencontainers.image.title=rspace-registry" \
  --label "org.opencontainers.image.version=${VERSION}" \
  --label "org.opencontainers.image.source=https://github.com/glennswest/rspace_registry" \
  "$ctr" >/dev/null
buildah commit "$ctr" "$IMG" >/dev/null
buildah rm "$ctr" >/dev/null
podman save --format oci-archive -o "$OUTDIR/rspace-registry-${VERSION}-${ARCH}.oci.tar" "localhost/${IMG}"

# ---- 5. Checksums + summary --------------------------------------------------
( cd "$OUTDIR" && sha256sum rspace-registry* > SHA256SUMS )
echo
echo ">> Artifacts in $OUTDIR:"
( cd "$OUTDIR" && ls -lh && echo && cat SHA256SUMS )
