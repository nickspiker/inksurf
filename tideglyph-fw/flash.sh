#!/usr/bin/env bash
# Build the firmware and flash it over the XIAO's UF2 bootloader.
#
# Usage: run this, then (if not already in bootloader mode) double-tap the RESET
# button on the XIAO. It waits for the bootloader drive to appear, then uf2conv
# deploys the image, which auto-flashes and reboots into the app.
set -euo pipefail

CRATE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ELF="$CRATE/target/thumbv7em-none-eabihf/release/tideglyph-fw"
BASE_ADDR="${BASE_ADDR:-0x27000}"         # must match memory.x FLASH ORIGIN (after the S140 SoftDevice)
FAMILY="0xADA52840"                        # nRF52840 UF2 family id

echo "==> building release"
cargo build --release --manifest-path "$CRATE/Cargo.toml"

echo "==> ELF → bin (base $BASE_ADDR)"
llvm-objcopy -O binary "$ELF" /tmp/tideglyph.bin

echo "==> waiting for the UF2 bootloader drive (double-tap RESET on the XIAO if needed)…"
DRIVE=""
for _ in $(seq 1 60); do
  for d in /run/media/"$USER"/*BOOT* /run/media/"$USER"/XIAO* /media/"$USER"/*BOOT* /mnt/*BOOT*; do
    [ -e "$d/INFO_UF2.TXT" ] && DRIVE="$d" && break 2
  done
  sleep 1
done
if [ -z "$DRIVE" ]; then
  echo "!! No UF2 drive appeared. Double-tap RESET and re-run." >&2
  exit 1
fi

echo "==> deploying to $DRIVE ($(grep -m1 Board-ID "$DRIVE/INFO_UF2.TXT" 2>/dev/null || echo UF2))"
# uf2conv writes the file AND flashes the mounted drive.
python3 /tmp/uf2conv.py /tmp/tideglyph.bin -b "$BASE_ADDR" -f "$FAMILY" -o /tmp/tideglyph.uf2
sync
echo "==> done — the board flashes and reboots into the app."
