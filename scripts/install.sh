#!/usr/bin/env sh
# superdeduper one-line installer.
#
# Usage:
#   curl -fsSL https://github.com/mickfixesjunk/superdeduper/raw/main/scripts/install.sh | sh
#
# What it does (in order):
#   1. Picks the right release tarball for your OS + arch.
#      (currently: Linux x86_64. macOS / FreeBSD planned for L1/L2.)
#   2. Downloads the tarball + the SHA256SUMS file from the
#      latest tagged release on GitHub.
#   3. Verifies the tarball SHA against SHA256SUMS. Aborts on mismatch.
#   4. Extracts to a temp dir.
#   5. Installs `superdeduper` + `superdeduper-gui` to:
#        - $SUPERDEDUPER_INSTALL_DIR if set (lets you override), else
#        - $HOME/.local/bin if it's on $PATH, else
#        - /usr/local/bin (asks for sudo if not writable).
#
# Environment overrides:
#   SUPERDEDUPER_VERSION  — install a specific tag instead of latest
#                           (e.g. SUPERDEDUPER_VERSION=v0.2.1 sh install.sh)
#   SUPERDEDUPER_INSTALL_DIR — target dir for the binaries (must exist)
#
# Exit codes:
#   0  — installed successfully
#   1  — generic failure (network, extract, etc.)
#   2  — checksum mismatch
#   3  — unsupported platform

set -e

REPO="mickfixesjunk/superdeduper"
BIN_NAMES="superdeduper superdeduper-gui"

# ----- Platform detection ----------------------------------------------
uname_os="$(uname -s)"
uname_arch="$(uname -m)"
case "$uname_os" in
  Linux)   os=linux ;;
  Darwin)
    echo "error: macOS install via one-liner not yet supported (L1 work)." >&2
    echo "       Once macOS binaries ship, this script will pick them up." >&2
    exit 3
    ;;
  FreeBSD)
    echo "error: FreeBSD install via one-liner not yet supported (L2 work)." >&2
    exit 3
    ;;
  *)
    echo "error: unsupported OS '$uname_os'." >&2
    exit 3
    ;;
esac

case "$uname_arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64)
    echo "error: ARM64 not yet supported on $os (planned)." >&2
    exit 3
    ;;
  *)
    echo "error: unsupported architecture '$uname_arch'." >&2
    exit 3
    ;;
esac

# ----- Version resolution ----------------------------------------------
# Allow override via env var (handy for pinning during testing). Default
# to whatever GH returns as the "latest" release — note that draft
# releases are NOT considered latest, so a v0.2.X release won't fire
# until someone publishes it. That's the safer default for end users.
if [ -n "${SUPERDEDUPER_VERSION:-}" ]; then
  version="$SUPERDEDUPER_VERSION"
else
  # `gh release view --json tagName` would be cleaner but assumes the
  # user has gh installed — they probably don't. Use the public
  # /releases/latest redirect instead: GitHub returns a 302 to
  # /releases/tag/<latest-tag>, and the tag is the last URL segment.
  redirect_url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
    "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)"
  version="${redirect_url##*/}"
  if [ -z "$version" ] || [ "$version" = "latest" ]; then
    echo "error: could not resolve latest version. Check ${REPO} has a published release." >&2
    exit 1
  fi
fi

tarball="superdeduper-${version}-${os}-${arch}.tar.gz"
base_url="https://github.com/${REPO}/releases/download/${version}"

# ----- Download + verify -----------------------------------------------
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

echo "==> Downloading ${tarball} (${version})…"
if ! curl -fsSL -o "$tmpdir/$tarball" "$base_url/$tarball"; then
  echo "error: download failed. URL: $base_url/$tarball" >&2
  echo "       (Possible: tag exists but artifact name format changed, or" >&2
  echo "        this OS+arch combo isn't on the release.)" >&2
  exit 1
fi

echo "==> Downloading SHA256SUMS…"
if ! curl -fsSL -o "$tmpdir/SHA256SUMS" "$base_url/SHA256SUMS"; then
  echo "error: SHA256SUMS not available on release ${version}. Aborting (won't ship unverified)." >&2
  exit 1
fi

echo "==> Verifying SHA-256…"
# `sha256sum -c` accepts either GNU coreutils or BusyBox. macOS lacks
# it by default but we already exited above for Darwin, so this path is
# Linux-only. Read SHA256SUMS for our specific tarball, recompute, diff.
expected="$(grep -E "[[:space:]]${tarball}\$" "$tmpdir/SHA256SUMS" | awk '{print $1}')"
if [ -z "$expected" ]; then
  echo "error: ${tarball} not listed in SHA256SUMS. Aborting." >&2
  exit 2
fi
actual="$(sha256sum "$tmpdir/$tarball" | awk '{print $1}')"
if [ "$expected" != "$actual" ]; then
  echo "error: SHA-256 mismatch — refusing to install." >&2
  echo "  expected: $expected" >&2
  echo "  actual:   $actual" >&2
  exit 2
fi

# ----- Extract ---------------------------------------------------------
echo "==> Extracting…"
tar -xzf "$tmpdir/$tarball" -C "$tmpdir"

# The release tarball layout is `superdeduper-<version>-<os>-<arch>/{superdeduper,superdeduper-gui,...}`.
extract_dir="$tmpdir/superdeduper-${version}-${os}-${arch}"
if [ ! -d "$extract_dir" ]; then
  echo "error: extracted dir ${extract_dir} not found. Tarball layout may have changed." >&2
  exit 1
fi

# ----- Install destination ---------------------------------------------
choose_install_dir() {
  if [ -n "${SUPERDEDUPER_INSTALL_DIR:-}" ]; then
    echo "$SUPERDEDUPER_INSTALL_DIR"
    return
  fi
  user_bin="$HOME/.local/bin"
  case ":$PATH:" in
    *":$user_bin:"*) echo "$user_bin"; return ;;
  esac
  echo "/usr/local/bin"
}

install_dir="$(choose_install_dir)"
mkdir -p "$install_dir" 2>/dev/null || true

needs_sudo=0
if [ ! -w "$install_dir" ]; then
  needs_sudo=1
fi

echo "==> Installing to ${install_dir}…"
for bin in $BIN_NAMES; do
  src="$extract_dir/$bin"
  if [ ! -f "$src" ]; then
    echo "warning: $bin not in tarball; skipping" >&2
    continue
  fi
  chmod +x "$src"
  if [ "$needs_sudo" = "1" ]; then
    sudo install -m 0755 "$src" "$install_dir/$bin"
  else
    install -m 0755 "$src" "$install_dir/$bin"
  fi
  echo "    installed $install_dir/$bin"
done

# ----- Done ------------------------------------------------------------
echo "==> Done. Try:"
echo "      superdeduper --version"
echo "      superdeduper scan ~/Downloads --format report"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    echo
    echo "note: ${install_dir} is not on your PATH. Add it with:"
    echo "  echo 'export PATH=\"${install_dir}:\$PATH\"' >> ~/.profile"
    ;;
esac
