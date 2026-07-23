/* tideglyph APP memory map — must match tideglyph-boot's partition addresses.
   The app is the ACTIVE image, launched by the embassy-boot second stage; its
   code lives at ACTIVE (0x08000) and it sets VTOR=0x08000 in pre_init (embassy-
   boot's load() also points VTOR here on handoff). DFU/STATE/MANIFEST are
   declared for the on-device OTA writer (FirmwareUpdater from_linkerfile + the
   signed manifest page). */
MEMORY
{
  FLASH            : ORIGIN = 0x00008000, LENGTH = 256K  /* = ACTIVE (running app) */
  BOOTLOADER_STATE : ORIGIN = 0x00007000, LENGTH = 4K
  DFU              : ORIGIN = 0x00048000, LENGTH = 260K
  MANIFEST         : ORIGIN = 0x00089000, LENGTH = 4K
  RAM        (rwx) : ORIGIN = 0x20000000, LENGTH = 256K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE);
__bootloader_state_end   = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE);

__bootloader_dfu_start = ORIGIN(DFU);
__bootloader_dfu_end   = ORIGIN(DFU) + LENGTH(DFU);

__manifest_start = ORIGIN(MANIFEST);
__manifest_end   = ORIGIN(MANIFEST) + LENGTH(MANIFEST);
