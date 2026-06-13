#!/usr/bin/env bash
# Build, install, and (re)start the inksurf daemon as a systemd service.
#
# The binary is installed to /usr/local/bin/inksurf rather than run from the
# cargo target dir: SELinux-enforcing hosts (Fedora et al.) label files under
# an arbitrary /mnt mount as unlabeled_t, which the service domain may not
# exec. /usr/local/bin is a properly-labeled (bin_t) location, so installing
# there sidesteps the whole problem. Re-run this after every release build.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="x86_64-unknown-linux-gnu"
BIN_SRC="$REPO/tide-display/target/$TARGET/release/tide-display"
BIN_DST="/usr/local/bin/inksurf"
UNIT_SRC="$REPO/deploy/inksurf.service"
UNIT_DST="/etc/systemd/system/inksurf.service"

echo "==> building release binary"
cargo build --manifest-path "$REPO/tide-display/Cargo.toml" --target "$TARGET" --release

echo "==> installing $BIN_DST"
sudo install -m755 "$BIN_SRC" "$BIN_DST"
command -v restorecon >/dev/null && sudo restorecon -v "$BIN_DST" || true

echo "==> installing $UNIT_DST"
sudo cp "$UNIT_SRC" "$UNIT_DST"
sudo systemctl daemon-reload
sudo systemctl reset-failed inksurf.service 2>/dev/null || true
sudo systemctl enable --now inksurf.service
sudo systemctl restart inksurf.service

echo "==> status"
systemctl is-active inksurf.service
journalctl -u inksurf -n 5 --no-pager
