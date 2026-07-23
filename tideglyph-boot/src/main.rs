//! tideglyph A/B second-stage bootloader.
//!
//! Launched by the MBR at 0x1000 (under the retained stock UF2 bootloader). On
//! each boot it inspects BOOTLOADER_STATE: if an update is pending it swaps
//! DFU<->ACTIVE with power-fail safety (progress journaled in STATE, WDT petted
//! throughout), then jumps to the ACTIVE app. If the app was just swapped in and
//! fails to call `mark_booted()` before the watchdog fires, the next boot reverts
//! to the previous image. Verification of update images is the APP's job (VSF /
//! Ed25519 / BLAKE3) — this bootloader is pure swap mechanism, no crypto.

#![no_std]
#![no_main]

use core::cell::RefCell;

use cortex_m_rt::{entry, exception};
use embassy_boot_nrf::*;
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::wdt::{self, HaltConfig, SleepConfig};
use embassy_sync::blocking_mutex::Mutex;

/// Runs before statics init. The MBR forwards here at 0x1000 without fixing VTOR,
/// so any fault/exception would otherwise vector through the MBR's table. Point
/// VTOR at our own vector table (FLASH ORIGIN 0x1000). embassy-boot's `load()`
/// re-points VTOR to ACTIVE when it jumps to the app, so this only covers the
/// bootloader's own brief execution.
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    (0xE000_ED08 as *mut u32).write_volatile(0x0000_1000);
}

#[entry]
fn main() -> ! {
    let p = embassy_nrf::init(Default::default());

    // Watchdog gates the app's trial boot: the bootloader starts it, pets it
    // during the (potentially long) swap via WatchdogFlash, then hands off. The
    // freshly-swapped app must pet it / mark_booted within the timeout or the
    // next boot reverts. ~5 s window.
    let mut wdt_config = wdt::Config::default();
    wdt_config.timeout_ticks = 32768 * 5;
    wdt_config.action_during_sleep = SleepConfig::Run;
    wdt_config.action_during_debug_halt = HaltConfig::Pause;

    let flash = WatchdogFlash::start(Nvmc::new(p.NVMC), p.WDT, wdt_config);
    let flash = Mutex::new(RefCell::new(flash));

    let config = BootLoaderConfig::from_linkerfile_blocking(&flash, &flash, &flash);
    let active_offset = config.active.offset();
    let bl: BootLoader = BootLoader::prepare(config);

    unsafe { bl.load(active_offset) }
}

#[unsafe(no_mangle)]
#[cfg_attr(target_os = "none", unsafe(link_section = ".HardFault.user"))]
unsafe extern "C" fn HardFault() {
    cortex_m::peripheral::SCB::sys_reset();
}

#[exception]
unsafe fn DefaultHandler(_: i16) -> ! {
    cortex_m::peripheral::SCB::sys_reset();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    cortex_m::asm::udf();
}
