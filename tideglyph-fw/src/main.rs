//! DIAGNOSTIC: minimal RGB color-cycle to answer one question — does our app
//! actually execute on this board?
//!
//! Direct register writes only (no HAL, no framebuffer, ~no RAM) — the same
//! approach as the first blink. It cycles the on-board RGB LED red → green →
//! blue forever. The board's hardware orange charge LED cannot change colour, so
//! if you see the colour cycling, the app is running (and the panel-driver
//! version's problem is in the HAL/framebuffer path). If you see only steady
//! orange, the app isn't booting at all (RAM/SoftDevice/boot issue) and no LED
//! pin choice will matter.

#![no_std]
#![no_main]

use cortex_m::asm::delay as cpu_delay;
use cortex_m_rt::entry;
use panic_halt as _;

// nRF52840 GPIO port P0.
const P0: u32 = 0x5000_0000;
const DIRSET: u32 = P0 + 0x518;
const OUTSET: u32 = P0 + 0x508; // write 1 → pin HIGH (LED off, active-low)
const OUTCLR: u32 = P0 + 0x50C; // write 1 → pin LOW  (LED on)

// XIAO nRF52840 RGB LED (active-low): red=P0.26, green=P0.30, blue=P0.06.
const RED: u32 = 26;
const GREEN: u32 = 30;
const BLUE: u32 = 6;

#[inline(always)]
fn w(addr: u32, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

#[entry]
fn main() -> ! {
    // All three LED pins as outputs.
    w(DIRSET, (1 << RED) | (1 << GREEN) | (1 << BLUE));
    // Start all off (drive high).
    w(OUTSET, (1 << RED) | (1 << GREEN) | (1 << BLUE));

    loop {
        for &pin in &[RED, GREEN, BLUE] {
            w(OUTSET, (1 << RED) | (1 << GREEN) | (1 << BLUE)); // all off
            w(OUTCLR, 1 << pin); // this one on
            cpu_delay(24_000_000); // ~0.4 s
        }
    }
}
