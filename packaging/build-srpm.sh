#!/bin/bash
# Build the SRPM (source tarball from a git tag + vendored cargo deps) and
# optionally submit it to COPR.
#
# Release flow:
#   1. Bump Cargo.toml version + spec Version (+ %changelog) — one commit.
#   2. git tag vX.Y.Z && git push --tags
#   3. ./packaging/build-srpm.sh [--copr]
#
# --head builds from HEAD instead of the tag (local testing only — never submit
# a --head build).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SPEC="$REPO/packaging/sni-watcher.spec"
SOURCES="${HOME}/rpmbuild/SOURCES"
COPR_PROJECT="${COPR_PROJECT:-sni-watcher}"

VER=$(sed -n 's/^Version:[[:space:]]*//p' "$SPEC")
CARGO_VER=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)
if [ "$VER" != "$CARGO_VER" ]; then
    echo "ERROR: spec Version ($VER) != Cargo.toml version ($CARGO_VER)" >&2
    exit 1
fi

REF="v$VER"
if [ "${1:-}" = "--head" ]; then
    REF="HEAD"
    echo "WARNING: building from HEAD (testing only)"
    shift
elif ! git -C "$REPO" rev-parse -q --verify "refs/tags/$REF" >/dev/null; then
    echo "ERROR: tag $REF not found — tag the release first (or use --head to test)" >&2
    exit 1
fi

mkdir -p "$SOURCES"
echo "==> source tarball from $REF"
git -C "$REPO" archive --format=tar.gz --prefix="sni-watcher-$VER/" \
    -o "$SOURCES/sni-watcher-$VER.tar.gz" "$REF"

echo "==> vendoring cargo dependencies"
VENDOR_DIR=$(mktemp -d)
trap 'rm -rf "$VENDOR_DIR"' EXIT
git -C "$REPO" archive --prefix=src/ "$REF" | tar -x -C "$VENDOR_DIR"
(cd "$VENDOR_DIR/src" && cargo vendor --locked >/dev/null)
tar -cJf "$SOURCES/sni-watcher-$VER-vendor.tar.xz" -C "$VENDOR_DIR/src" vendor

echo "==> building SRPM"
SRPM=$(rpmbuild -bs "$SPEC" | sed -n 's/^Wrote: //p')
echo "    $SRPM"
rpmlint "$SRPM" || true

if [ "${1:-}" = "--copr" ]; then
    echo "==> submitting to COPR project $COPR_PROJECT"
    if ! copr-cli build "$COPR_PROJECT" "$SRPM"; then
        echo "ERROR: copr build failed. If this was a 401, the API token has" >&2
        echo "expired (~180 days) — renew at https://copr.fedorainfracloud.org/api/" >&2
        exit 1
    fi
fi
