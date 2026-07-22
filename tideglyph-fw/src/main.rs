//! P0 flash-loop validation: blink the XIAO nRF52840 user LED (bare metal).
//!
//! No embassy yet — this exists only to prove the toolchain end to end: thumbv7em build → UF2 conversion → double-tap-reset flash → the app actually boots at the memory.x origin and runs. Once the red LED blinks, the flash path and memory layout are confirmed and the panel driver (embassy + JD79667) can follow with confidence.

#![no_std]
#![no_main]

use cortex_m_rt::entry;
use panic_halt as _;

// nRF52840 GPIO port P0.
const P0_BASE: u32 = 0x5000_0000;
const DIRSET: u32 = P0_BASE + 0x518;
const OUTSET: u32 = P0_BASE + 0x508;
const OUTCLR: u32 = P0_BASE + 0x50C;

// XIAO nRF52840 on-board RGB LED (active LOW). Red = P0.26.
const LED_RED: u32 = 26;

#[inline(always)]
fn write_reg(addr: u32, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

#[entry]
fn main() -> ! {
    // Configure the red LED pin as an output.
    write_reg(DIRSET, 1 << LED_RED);

    loop {
        // Active-low: drive low = LED on.
        write_reg(OUTCLR, 1 << LED_RED);
        cortex_m::asm::delay(16_000_000); // ~0.25 s at 64 MHz
        // Drive high = LED off.
        write_reg(OUTSET, 1 << LED_RED);
        cortex_m::asm::delay(16_000_000);
    }
}
