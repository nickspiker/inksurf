/* nRF52840 (1 MB flash, 256 KB RAM) with the Adafruit/Seeed UF2 bootloader.
 *
 * The MBR lives at 0x0..0x1000 and the UF2 bootloader at 0xF4000..0x100000 —
 * we must not overwrite either. With no SoftDevice, the application starts right
 * after the MBR at 0x1000. (If the board fails to boot into this layout, the
 * other common origin is 0x27000 — the SoftDevice-sized gap; double-tap RESET
 * always recovers into the UF2 bootloader, so this is safe to iterate.)
 */
MEMORY
{
    FLASH : ORIGIN = 0x00001000, LENGTH = 0xF3000
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}
