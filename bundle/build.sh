#!/usr/bin/env bash
# Builds the Linux x86_64 release bundle.
#
#   bundle/build.sh <output dir>
#
# Produces <output dir>/wn-server-<version>-linux-amd64.tar.gz. VERSION in the
# environment sets <version>; otherwise it is `git describe`, as in the
# Makefile. WN_ACOUSTID_KEY in the environment sets the AcoustID key compiled
# into wn-server; it is empty by default. Requires bash, GNU tar, curl, sha256sum, install, grep, mktemp, go, and uv. Runs on Linux.
set -euo pipefail

if [[ $# -ne 1 ]]; then
	echo "usage: $0 <output dir>" >&2
	exit 2
fi

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$1"
out=$(cd "$1" && pwd)

# shellcheck source=bundle/versions.env
source "$repo/bundle/versions.env"

VERSION=${VERSION:-$(git -C "$repo" describe --tags --always --dirty 2>/dev/null || echo dev)}
name="wn-server-${VERSION}-linux-amd64"

for tool in go curl tar sha256sum uv; do
	if ! command -v "$tool" >/dev/null; then
		echo "build.sh: $tool not found on PATH" >&2
		exit 1
	fi
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage="$work/$name"
dl="$work/download"
mkdir -p "$stage" "$dl"

fetch() {
	curl --fail --silent --show-error --location --retry 3 -o "$2" "$1"
}

echo "==> wn-server $VERSION"
(
	cd "$repo"
	CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath \
		-ldflags "-X github.com/waxnerdhq/wn-server/internal/cli.Version=${VERSION} -X github.com/waxnerdhq/wn-server/internal/beets.DefaultAcoustIDKey=${WN_ACOUSTID_KEY:-}" \
		-o "$stage/wn-server" ./cmd/wn-server
)

echo "==> navidrome $NAVIDROME_VERSION"
nd_asset="navidrome_${NAVIDROME_VERSION}_linux_amd64.tar.gz"
nd_base="https://github.com/navidrome/navidrome/releases/download/v${NAVIDROME_VERSION}"
fetch "$nd_base/$nd_asset" "$dl/$nd_asset"
fetch "$nd_base/navidrome_checksums.txt" "$dl/navidrome_checksums.txt"
grep -E "^[0-9a-f]{64}  ${nd_asset}\$" "$dl/navidrome_checksums.txt" >"$dl/navidrome.sha256" || {
	echo "build.sh: $nd_asset not listed in navidrome_checksums.txt" >&2
	exit 1
}
(cd "$dl" && sha256sum --check --strict navidrome.sha256)
mkdir "$dl/navidrome"
tar -xzf "$dl/$nd_asset" -C "$dl/navidrome" navidrome
install -m 0755 "$dl/navidrome/navidrome" "$stage/navidrome"

echo "==> fpcalc $CHROMAPRINT_VERSION"
fp_dir="chromaprint-fpcalc-${CHROMAPRINT_VERSION}-linux-x86_64"
fp_asset="${fp_dir}.tar.gz"
fetch "https://github.com/acoustid/chromaprint/releases/download/v${CHROMAPRINT_VERSION}/$fp_asset" "$dl/$fp_asset"
grep -E "^[0-9a-f]{64}  ${fp_asset}\$" "$repo/bundle/checksums" >"$dl/fpcalc.sha256" || {
	echo "build.sh: $fp_asset has no sha256 in bundle/checksums" >&2
	exit 1
}
(cd "$dl" && sha256sum --check --strict fpcalc.sha256)
tar -xzf "$dl/$fp_asset" -C "$dl" "$fp_dir/fpcalc"
install -m 0755 "$dl/$fp_dir/fpcalc" "$stage/fpcalc"

echo "==> beets $BEETS_VERSION (pex scie, Python $PYTHON_VERSION)"
# pex resolves wheels for the interpreter it runs under, so it runs under the
# same Python version that the scie carries. uv provides that interpreter.
uvx --python "$PYTHON_VERSION" --python-preference only-managed \
	--from "pex==${PEX_VERSION}" pex \
	"beets[chroma]==${BEETS_VERSION}" requests \
	--console-script beet \
	--scie eager \
	--scie-only \
	--scie-pbs-release "$PBS_RELEASE" \
	--scie-python-version "$PYTHON_VERSION" \
	--scie-pbs-stripped \
	-o "$dl/beets.pex"
install -m 0755 "$dl/beets" "$stage/beets"
mkdir "$stage/beets-plugins"
cp "$repo/bundle/beets-plugins/wn_candidates.py" "$stage/beets-plugins/wn_candidates.py"

cp "$repo/LICENSE" "$stage/LICENSE"
cp "$repo/bundle/NOTICE" "$stage/NOTICE"

# The binaries run only on Linux x86_64. Check them there.
if [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]]; then
	echo "==> smoke test"
	"$stage/wn-server" version
	nd_out=$("$stage/navidrome" --version)
	if [[ $nd_out != "${NAVIDROME_VERSION} "* ]]; then
		echo "build.sh: navidrome --version printed '$nd_out', want $NAVIDROME_VERSION" >&2
		exit 1
	fi
	"$stage/fpcalc" -version
	# Loading the chroma plugin proves that pyacoustid is inside the scie.
	beets_out=$(SCIE_BASE="$work/scie" BEETSDIR="$work/beetsdir" "$stage/beets" -p chroma version)
	echo "$beets_out"
	if [[ $beets_out != "beets version ${BEETS_VERSION}"* || $beets_out != *chroma* ]]; then
		echo "build.sh: beets -p chroma version does not report $BEETS_VERSION with chroma" >&2
		exit 1
	fi
fi

tarball="$out/${name}.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner \
	--mtime="@${SOURCE_DATE_EPOCH:-$(git -C "$repo" log -1 --format=%ct 2>/dev/null || date +%s)}" \
	-C "$work" -czf "$tarball" "$name"
(cd "$out" && sha256sum "${name}.tar.gz" >"${name}.tar.gz.sha256")
echo "==> $tarball"
tar -tzvf "$tarball"
