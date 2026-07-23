/* tideglyph A/B partition map (shared by tideglyph-boot and tideglyph-fw — the
   app's memory.x must place ACTIVE/DFU/STATE at the SAME addresses).

   reset -> MBR(0x0) -> this bootloader(0x1000) -> swap if pending -> ACTIVE app.
   OTA only ever writes DFU + STATE (+ the app's MANIFEST page at 0x89000, which
   the bootloader doesn't need to know about). Bootloader, MBR, and the stock UF2
   bootloader (0xF4000) are never touched over the air. */
MEMORY
{
  FLASH            : ORIGIN = 0x00001000, LENGTH = 24K   /* bootloader code            */
  BOOTLOADER_STATE : ORIGIN = 0x00007000, LENGTH = 4K    /* swap magic + progress      */
  ACTIVE           : ORIGIN = 0x00008000, LENGTH = 256K  /* running app                */
  DFU              : ORIGIN = 0x00048000, LENGTH = 260K  /* staging = ACTIVE + 1 page  */
  RAM        (rwx) : ORIGIN = 0x20000000, LENGTH = 32K   /* bootloader-only; app gets all 256K */
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE);
__bootloader_state_end   = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE);

__bootloader_active_start = ORIGIN(ACTIVE);
__bootloader_active_end   = ORIGIN(ACTIVE) + LENGTH(ACTIVE);

__bootloader_dfu_start = ORIGIN(DFU);
__bootloader_dfu_end   = ORIGIN(DFU) + LENGTH(DFU);
