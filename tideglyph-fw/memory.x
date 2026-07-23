/* XIAO nRF52840 with the stock Seeed/Adafruit UF2 bootloader: MBR at 0x0
   forwards to the app at 0x1000 (bare, no SoftDevice — the S140 was overwritten
   deliberately; the SoftDevice-forwarded boot faults bare apps). Bootloader
   occupies 0xF4000..0x100000, so FLASH ends at 0xF2000 and a STORAGE region
   (bonds/config, and later the OTA staging metadata) sits at 0xF2000..0xF4000.
   NOTE: the app MUST set VTOR = 0x1000 in pre_init — the MBR forwards
   interrupts to the wrong table for bare apps and the first interrupt otherwise
   locks up the chip (crash-boot loop). Double-tap reset always recovers DFU. */
MEMORY
{
  FLASH   : ORIGIN = 0x00001000, LENGTH = 0xF1000
  STORAGE : ORIGIN = 0x000F2000, LENGTH = 8K
  RAM     : ORIGIN = 0x20000000, LENGTH = 256K
}

__storage_start = ORIGIN(STORAGE);
__storage_end = ORIGIN(STORAGE) + LENGTH(STORAGE);
