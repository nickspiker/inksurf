#!/usr/bin/env bash
# Build the firmware and flash it over the XIAO's UF2 bootloader.
#
# Usage: double-tap the RESET button on the XIAO (it mounts a USB drive with
# INFO_UF2.TXT), then run this script. It builds, converts to UF2, and copies to
# the bootloader drive, which auto-flashes and reboots into the app.
set -euo pipefail

CRATE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ELF="$CRATE/target/thumbv7em-none-eabihf/release/tideglyph-fw"
BASE_ADDR="${BASE_ADDR:-0x1000}"          # must match memory.x FLASH ORIGIN
FAMILY="0xADA52840"                        # nRF52840 UF2 family id

echo "==> building release"
cargo build --release --manifest-path "$CRATE/Cargo.toml"

echo "==> ELF → bin → uf2 (base $BASE_ADDR)"
llvm-objcopy -O binary "$ELF" /tmp/tideglyph.bin
python3 /tmp/uf2conv.py /tmp/tideglyph.bin -b "$BASE_ADDR" -f "$FAMILY" -o /tmp/tideglyph.uf2

echo "==> locating UF2 bootloader drive (look for INFO_UF2.TXT)"
DRIVE=""
for d in /run/media/"$USER"/* /media/"$USER"/* /mnt/*; do
  [ -f "$d/INFO_UF2.TXT" ] && DRIVE="$d" && break
done
if [ -z "$DRIVE" ]; then
  echo "!! No UF2 drive found. Double-tap RESET on the XIAO and re-run." >&2
  echo "   (UF2 image is ready at /tmp/tideglyph.uf2 — copy it manually if needed.)" >&2
  exit 1
fi

echo "==> copying to $DRIVE"
cp /tmp/tideglyph.uf2 "$DRIVE/"
sync
echo "==> done — the board will flash and reboot into the app."
