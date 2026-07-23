//! P0 panel bring-up: drive the Seeed 2.9" BWRY (JD79667, 128×296) panel and
//! push a 4-color-band test frame (black / white / yellow / red).
//!
//! This is the panel equivalent of the blink: it validates the SPI wiring, the
//! JD79667 init sequence, the DTM framebuffer stream, and the refresh — and the
//! bands confirm the 2-bit colour mapping in one shot before we render tides.
//!
//! Driver facts reused from inksurf's working JD79667 panel (src/panel_jd79667.rs):
//! DTM=0x10, DRF=0x12+[0x00], POWER_ON=0x04, POWER_OFF=0x02, and BUSY is LOW
//! while the chip is busy (wait for it to go HIGH). Init sequence + TRES geometry
//! from Seeed's Setup512 for this specific 128×296 BWRY panel.

#![no_std]
#![no_main]

use cortex_m::asm::delay as cpu_delay;
use cortex_m_rt::entry;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiBus;
use nrf52840_hal as hal;
use panic_halt as _;

use hal::gpio::{p0, p1, Level};
use hal::spim::{Frequency, Pins, Spim, MODE_0};

const W: usize = 128;
const H: usize = 296;
const ROW_BYTES: usize = W / 4; // 2 bits/px, 4 px/byte = 32
const FB_BYTES: usize = ROW_BYTES * H; // 9472

// JD79667 2-bit colour codes (from inksurf's working panel), packed 4/byte.
const BLACK: u8 = 0b00;
const WHITE: u8 = 0b01;
const YELLOW: u8 = 0b10;
const RED: u8 = 0b11;

// ~64 MHz core → cycles per millisecond. Init delays only need a lower bound, so
// exactness doesn't matter.
fn delay_ms(ms: u32) {
    cpu_delay(64_000 * ms);
}

/// Wait for the chip to signal ready (BUSY goes HIGH), bounded so a mis-wired or
/// floating BUSY line can't deadlock init with no feedback (~5 s cap).
fn wait_ready<P: InputPin>(busy: &mut P) {
    for _ in 0..5000 {
        if busy.is_high().unwrap_or(true) {
            return;
        }
        delay_ms(1);
    }
}

// Framebuffer in RAM (SPIM EasyDMA can't stream from flash).
static mut FB: [u8; FB_BYTES] = [0; FB_BYTES];

#[entry]
fn main() -> ! {
    let p = hal::pac::Peripherals::take().unwrap();
    let port0 = p0::Parts::new(p.P0);
    let port1 = p1::Parts::new(p.P1);

    // Diagnostic LED (red, P0.26, active-low): solid ON while the panel sequence
    // runs; a slow heartbeat once it completes. Solid-and-never-heartbeats means
    // stuck (a BUSY wait maxed out or a panic); heartbeat means every SPI write
    // + refresh finished. This is our signal independent of the panel itself.
    let mut led = port0.p0_26.into_push_pull_output(Level::High).degrade();
    let _ = led.set_low(); // ON

    // SPI pins: SCLK=P1.13 (D8), MOSI=P1.15 (D10). Panel is write-only (no MISO).
    let sck = port1.p1_13.into_push_pull_output(Level::Low).degrade();
    let mosi = port1.p1_15.into_push_pull_output(Level::Low).degrade();
    let mut spi = Spim::new(
        p.SPIM0,
        Pins { sck: Some(sck), mosi: Some(mosi), miso: None },
        Frequency::M8,
        MODE_0,
        0,
    );

    // Control pins on the EN04/EN05 board (D-label → nRF GPIO from the Plus
    // variant): CS=D7=P1.12, DC=D16=P0.31, RST=D11=P0.15, BUSY=D3=P0.29,
    // and the panel power-enable EN=D6=P1.11 (must be driven HIGH to power it).
    let mut panel_en = port1.p1_11.into_push_pull_output(Level::Low).degrade();
    let mut cs = port1.p1_12.into_push_pull_output(Level::High).degrade();
    let mut dc = port0.p0_31.into_push_pull_output(Level::Low).degrade();
    let mut rst = port0.p0_15.into_push_pull_output(Level::High).degrade();
    let mut busy = port0.p0_29.into_floating_input().degrade();

    // Power the panel, let its rails settle, then hardware reset.
    let _ = panel_en.set_high();
    delay_ms(10);
    let _ = rst.set_low();
    delay_ms(20);
    let _ = rst.set_high();
    delay_ms(50);
    wait_ready(&mut busy);

    // Init sequence (Seeed Setup512, JD79667, 128×296 BWRY).
    let cmd = |spi: &mut Spim<hal::pac::SPIM0>, cs: &mut _, dc: &mut _, c: u8, data: &[u8]| {
        // command byte
        let _ = OutputPin::set_low(dc as &mut _);
        let _ = OutputPin::set_low(cs as &mut _);
        let _ = SpiBus::write(spi, &[c]);
        // data bytes (DC high)
        let _ = OutputPin::set_high(dc as &mut _);
        for &b in data {
            let _ = SpiBus::write(spi, &[b]);
        }
        let _ = OutputPin::set_high(cs as &mut _);
    };

    cmd(&mut spi, &mut cs, &mut dc, 0x4D, &[0x78]);
    cmd(&mut spi, &mut cs, &mut dc, 0x00, &[0x0F, 0x29]); // PSR
    cmd(&mut spi, &mut cs, &mut dc, 0x01, &[0x07, 0x00]); // PWRR
    cmd(&mut spi, &mut cs, &mut dc, 0x03, &[0x10, 0x54, 0x44]); // POFS
    cmd(&mut spi, &mut cs, &mut dc, 0x06, &[0x05, 0x00, 0x3F, 0x0A, 0x25, 0x12, 0x1A]); // BTST
    cmd(&mut spi, &mut cs, &mut dc, 0x50, &[0x37]); // CDI
    cmd(&mut spi, &mut cs, &mut dc, 0x60, &[0x02, 0x02]); // TCON
    cmd(&mut spi, &mut cs, &mut dc, 0x61, &[0x00, 0x80, 0x01, 0x28]); // TRES 128×296
    cmd(&mut spi, &mut cs, &mut dc, 0xE7, &[0x1C]);
    cmd(&mut spi, &mut cs, &mut dc, 0xE3, &[0x22]);
    cmd(&mut spi, &mut cs, &mut dc, 0xB4, &[0xD0]);
    cmd(&mut spi, &mut cs, &mut dc, 0xB5, &[0x03]);
    cmd(&mut spi, &mut cs, &mut dc, 0xE9, &[0x01]);
    cmd(&mut spi, &mut cs, &mut dc, 0x30, &[0x08]); // PLL
    cmd(&mut spi, &mut cs, &mut dc, 0x04, &[]); // POWER ON
    wait_ready(&mut busy);

    // Build the 4-band test frame: black / white / yellow / red, top to bottom.
    let fb = unsafe { &mut *core::ptr::addr_of_mut!(FB) };
    for row in 0..H {
        let color = match row * 4 / H {
            0 => BLACK,
            1 => WHITE,
            2 => YELLOW,
            _ => RED,
        };
        let byte = (color << 6) | (color << 4) | (color << 2) | color;
        for c in 0..ROW_BYTES {
            fb[row * ROW_BYTES + c] = byte;
        }
    }

    // Stream the framebuffer (DTM = 0x10), then refresh (DRF = 0x12 + [0x00]).
    let _ = OutputPin::set_low(&mut dc);
    let _ = OutputPin::set_low(&mut cs);
    let _ = SpiBus::write(&mut spi, &[0x10]);
    let _ = OutputPin::set_high(&mut dc);
    let _ = SpiBus::write(&mut spi, &fb[..]);
    let _ = OutputPin::set_high(&mut cs);

    cmd(&mut spi, &mut cs, &mut dc, 0x12, &[0x00]); // DRF refresh
    delay_ms(50);
    wait_ready(&mut busy);

    cmd(&mut spi, &mut cs, &mut dc, 0x02, &[]); // POWER OFF

    // Completed the full sequence — heartbeat the LED so we can tell "ran to the
    // end" apart from "stuck mid-init".
    loop {
        let _ = led.set_low();
        delay_ms(150);
        let _ = led.set_high();
        delay_ms(850);
    }
}
