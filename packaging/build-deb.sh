#!/bin/sh
# Build shr-rs_<v>-1_amd64.deb and cockpit-shr-rs_<v>-1_all.deb from the two
# release tarballs.
#
# dpkg-deb over a staged tree, not debhelper: there is nothing to compile here
# (the engine tarball already holds a statically linked musl binary, the
# plugin tarball already holds the built JavaScript), so a debian/rules that
# only copied files would be ceremony around two `install` calls. A script
# also means a failing CI job can be reproduced by hand with one command in a
# `debian:13-slim` container.
#
# Usage: packaging/build-deb.sh VERSION ENGINE_TAR PLUGIN_TAR [OUTDIR]
# Needs: dpkg-deb (dpkg), tar, xz-utils, make, gettext (msgfmt), findutils.
set -eu

VERSION=${1:?usage: build-deb.sh VERSION ENGINE_TAR PLUGIN_TAR [OUTDIR]}
ENGINE_TAR=${2:?missing engine tarball}
PLUGIN_TAR=${3:?missing plugin tarball}
OUTDIR=${4:-$PWD}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ENGINE_TAR=$(cd "$(dirname "$ENGINE_TAR")" && pwd)/$(basename "$ENGINE_TAR")
PLUGIN_TAR=$(cd "$(dirname "$PLUGIN_TAR")" && pwd)/$(basename "$PLUGIN_TAR")
mkdir -p "$OUTDIR"
OUTDIR=$(cd "$OUTDIR" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

set -x

# ---- shr-rs (the engine) ---------------------------------------------------

tar xzf "$ENGINE_TAR" -C "$work"
src="$work/shr-rs-$VERSION"
stage="$work/stage-shr-rs"
deb="$ROOT/packaging/debian/shr-rs"

install -d -m0755 "$stage/DEBIAN"
sed "s/%{VERSION}/$VERSION/" "$deb/control.in" > "$stage/DEBIAN/control"
for script in postinst prerm postrm; do
    install -m0755 "$deb/$script" "$stage/DEBIAN/$script"
done

install -Dm0755 "$src/shr-rs" "$stage/usr/bin/shr-rs"
# /usr/lib, not /lib: Debian 13 completed the /usr merge and rejects a package
# shipping the aliased path.
install -Dm0644 "$src/packaging/shr-rs.service" \
    "$stage/usr/lib/systemd/system/shr-rs.service"
install -d -m0700 "$stage/var/lib/shr-rs"
install -d -m0755 "$stage/etc/shr-rs"
install -Dm0644 "$deb/copyright" "$stage/usr/share/doc/shr-rs/copyright"
install -Dm0644 "$src/README.md" "$stage/usr/share/doc/shr-rs/README.md"
install -Dm0644 "$src/LICENSE-MIT" "$stage/usr/share/doc/shr-rs/LICENSE-MIT"
install -Dm0644 "$src/LICENSE-APACHE" "$stage/usr/share/doc/shr-rs/LICENSE-APACHE"
# The binary is statically linked, so the crates' MIT/Apache notices have to
# travel with it. Generated into the tarball by the release workflow.
install -Dm0644 "$src/THIRD-PARTY-NOTICES.txt" \
    "$stage/usr/share/doc/shr-rs/THIRD-PARTY-NOTICES.txt"

dpkg-deb --root-owner-group --build "$stage" \
    "$OUTDIR/shr-rs_${VERSION}-1_amd64.deb"

# ---- cockpit-shr-rs (the plugin) -------------------------------------------

tar xf "$PLUGIN_TAR" -C "$work"
psrc="$work/cockpit-shr-rs"
pstage="$work/stage-cockpit-shr-rs"
pdeb="$ROOT/cockpit/packaging/debian"

# tar gives every entry the same mtime, so make would see the built bundle as
# no newer than its sources and could decide to re-run build.js -- which needs
# node, deliberately absent here. Stamping the two targets forward makes that
# impossible rather than merely unlikely. Order matters: the repo stamp is a
# prerequisite of the bundle.
touch "$psrc/pkg/lib/cockpit-po-plugin.js"
touch "$psrc/runtime-npm-modules.txt"

install -d -m0755 "$pstage/DEBIAN"
sed "s/%{VERSION}/$VERSION/" "$pdeb/control.in" > "$pstage/DEBIAN/control"

make -C "$psrc" install DESTDIR="$pstage" PREFIX=/usr
# Source maps are large and only useful when debugging the bundle; the rpm
# spec drops them in %install too.
find "$pstage/usr/share/cockpit" -name '*.map' -delete
install -Dm0644 "$pdeb/copyright" \
    "$pstage/usr/share/doc/cockpit-shr-rs/copyright"
install -Dm0644 "$psrc/README.md" \
    "$pstage/usr/share/doc/cockpit-shr-rs/README.md"
install -Dm0644 "$psrc/LICENSE" "$pstage/usr/share/doc/cockpit-shr-rs/LICENSE"
# Bundled third-party code and webfonts; same pair the rpm puts in %license.
install -Dm0644 "$psrc/dist/THIRD-PARTY-NOTICES.txt" \
    "$pstage/usr/share/doc/cockpit-shr-rs/THIRD-PARTY-NOTICES.txt"
install -Dm0644 "$psrc/dist/index.js.LEGAL.txt" \
    "$pstage/usr/share/doc/cockpit-shr-rs/index.js.LEGAL.txt"

dpkg-deb --root-owner-group --build "$pstage" \
    "$OUTDIR/cockpit-shr-rs_${VERSION}-1_all.deb"

set +x
ls -l "$OUTDIR"/*.deb
