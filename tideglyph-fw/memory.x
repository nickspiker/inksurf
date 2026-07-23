/* nRF52840 (1 MB flash, 256 KB RAM) with the Seeed XIAO UF2 bootloader.
 *
 * Flash layout (confirmed from the board's INFO_UF2.TXT):
 *   0x00000..0x01000  MBR
 *   0x01000..0x27000  SoftDevice S140 7.3.0  (present — do NOT overwrite)
 *   0x27000..0xF4000  application            <- us
 *   0xF4000..0x100000 UF2 bootloader
 *
 * The app therefore starts at 0x27000, after the SoftDevice. (We don't enable
 * the SoftDevice for a bare app, but it must stay intact in flash or the boot
 * chain breaks.) RAM: the SoftDevice reserves the low region only when enabled;
 * since we don't enable it, the app uses all of RAM.
 */
MEMORY
{
    /* Run BARE (no SoftDevice). The app is flashed at 0x1000, right after the
       MBR, overwriting the S140 SoftDevice — the bootloader detects "no SD" and
       boots us directly, so nothing forwards through the SoftDevice and faults
       us. Bootloader (0xF4000) and MBR (0x0) are untouched; double-tap reset
       still recovers to UF2 DFU. Full RAM is ours. */
    FLASH : ORIGIN = 0x00001000, LENGTH = 0xF3000
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}
