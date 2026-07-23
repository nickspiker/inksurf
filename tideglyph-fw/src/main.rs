//! P0 panel bring-up: drive the **Seeed 1.54" mono** panel (SSD1681, 200×200, black/white — model GDEY0154D67) via the Seeed nRF52840 + EN04 board, and push a black/white band test frame.
//!
//! This is a monochrome SSD1681 panel, NOT a BWRY colour panel — the display is black & white only. Config from Seeed's Setup505 (SSD1681): 1 bit/pixel, RAMWR = 0x24, full refresh = 0x22[0xF7] + 0x20. BUSY polarity is INVERTED vs the JD79xxx panels: HIGH = busy, LOW = ready. Pins are the Seeed EN04 board's nRF GPIO — matched pair, so pin compatibility is guaranteed.

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


// Diagnostic LED on D9 = P1.14 (the SPI MISO line — unused by a write-only
// e-paper panel, so a safe sacrificial pin). Driven by DIRECT registers
// (independent of the HAL, so it reports even if the HAL is what faults).
// Active high: drive high = LED on. Wire an LED + ~470Ω from D9 to GND.
const P1_BASE: u32 = 0x5000_0300;
const P0_DIRSET: u32 = P1_BASE + 0x518;
const P0_OUTSET: u32 = P1_BASE + 0x508;
const P0_OUTCLR: u32 = P1_BASE + 0x50C;
const LED_PIN: u32 = 14; // P1.14 = D9

#[inline(always)]
fn led_init() {
    unsafe { core::ptr::write_volatile(P0_DIRSET as *mut u32, 1 << LED_PIN) }
}
#[inline(always)]
fn led(on: bool) {
    let reg = if on { P0_OUTSET } else { P0_OUTCLR };
    unsafe { core::ptr::write_volatile(reg as *mut u32, 1 << LED_PIN) }
}
fn led_blink(times: u32) {
    for _ in 0..times {
        led(true);
        delay_ms(120);
        led(false);
        delay_ms(120);
    }
}

// SSD1681 mono: 200×200, 1 bit/pixel, 8 px/byte.
const W: usize = 200;
const H: usize = 200;
const ROW_BYTES: usize = W / 8; // 25
const FB_BYTES: usize = ROW_BYTES * H; // 5000

// ~64 MHz core → cycles per millisecond. Init delays only need a lower bound, so exactness doesn't matter.
fn delay_ms(ms: u32) {
    cpu_delay(64_000 * ms);
}

/// Wait for the chip to signal ready. SSD1681 BUSY is HIGH while busy, so wait until it goes LOW. Bounded (~5 s) so a mis-wired or floating BUSY can't deadlock with no feedback.
fn wait_ready<P: InputPin>(busy: &mut P) {
    for _ in 0..5000 {
        if busy.is_low().unwrap_or(true) {
            return;
        }
        delay_ms(1);
    }
}

// Framebuffer in RAM (SPIM EasyDMA can't stream from flash).
static mut FB: [u8; FB_BYTES] = [0; FB_BYTES];

#[entry]
fn main() -> ! {
    // Stage 1: app booted (direct registers, before any HAL). If you never see
    // even 1 blink, the app isn't running / LED is miswired.
    led_init();
    led_blink(1);
    delay_ms(800);

    let p = hal::pac::Peripherals::take().unwrap();
    let port0 = p0::Parts::new(p.P0);
    let port1 = p1::Parts::new(p.P1);

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

    // Stage 2: peripherals + Spim::new survived (the HAL SPI init is a fault suspect).
    led_blink(2);
    delay_ms(800);

    // Control pins on the EN04/EN05 board: CS=D7=P1.12, DC=D16=P0.31, RST=D11=P0.15, BUSY=D3=P0.29, panel power-enable EN=D6=P1.11 (driven HIGH).
    let mut panel_en = port1.p1_11.into_push_pull_output(Level::Low).degrade();
    let mut cs = port1.p1_12.into_push_pull_output(Level::High).degrade();
    let mut dc = port0.p0_31.into_push_pull_output(Level::Low).degrade();
    let mut rst = port0.p0_15.into_push_pull_output(Level::High).degrade();
    let mut busy = port0.p0_29.into_floating_input().degrade();

    // Power the panel, settle, hardware reset.
    let _ = panel_en.set_high();
    delay_ms(10);
    let _ = rst.set_low();
    delay_ms(10);
    let _ = rst.set_high();
    delay_ms(10);
    wait_ready(&mut busy);

    let cmd = |spi: &mut Spim<hal::pac::SPIM0>, cs: &mut _, dc: &mut _, c: u8, data: &[u8]| {
        let _ = OutputPin::set_low(dc as &mut _);
        let _ = OutputPin::set_low(cs as &mut _);
        let _ = SpiBus::write(spi, &[c]);
        let _ = OutputPin::set_high(dc as &mut _);
        for &b in data {
            let _ = SpiBus::write(spi, &[b]);
        }
        let _ = OutputPin::set_high(cs as &mut _);
    };

    // Init (Seeed Setup505 / SSD1681, 200×200).
    cmd(&mut spi, &mut cs, &mut dc, 0x12, &[]); // software reset
    wait_ready(&mut busy);
    cmd(&mut spi, &mut cs, &mut dc, 0x01, &[(H as u16 - 1) as u8, ((H as u16 - 1) >> 8) as u8, 0x00]); // driver output control
    cmd(&mut spi, &mut cs, &mut dc, 0x11, &[0x03]); // data entry mode: X inc, Y inc
    cmd(&mut spi, &mut cs, &mut dc, 0x44, &[0x00, (W / 8 - 1) as u8]); // RAM X window
    cmd(&mut spi, &mut cs, &mut dc, 0x45, &[0x00, 0x00, (H as u16 - 1) as u8, ((H as u16 - 1) >> 8) as u8]); // RAM Y window
    cmd(&mut spi, &mut cs, &mut dc, 0x3C, &[0x05]); // border = white
    cmd(&mut spi, &mut cs, &mut dc, 0x21, &[0x00, 0x80]); // display update control 1 (from inksurf's proven SSD1680 init)
    cmd(&mut spi, &mut cs, &mut dc, 0x18, &[0x80]); // temperature sensor
    cmd(&mut spi, &mut cs, &mut dc, 0x4E, &[0x00]); // RAM X address counter
    cmd(&mut spi, &mut cs, &mut dc, 0x4F, &[0x00, 0x00]); // RAM Y address counter
    wait_ready(&mut busy);

    // Stage 3: panel reset + full init sequence completed (all those SPI writes went out and the post-power-on BUSY wait returned).
    led_blink(3);
    delay_ms(800);

    // DIAGNOSTIC: flip the whole panel black ↔ white forever, with a single LED blip per cycle. LED blipping steadily = the loop (SPI sequence) runs every iteration; LED stuck after 3 = faulted in init; stuck after 2 = faulted at Spim::new.
    let fb = unsafe { &mut *core::ptr::addr_of_mut!(FB) };
    let mut fill: u8 = 0x00;
    loop {
        led(true);
        delay_ms(150);
        led(false);
        for b in fb.iter_mut() {
            *b = fill;
        }
        // Write B/W RAM (0x24) with the fill, then RED RAM (0x26) blanked to
        // zeros — inksurf's proven flow. Reset the RAM address counter to (0,0)
        // before each write.
        cmd(&mut spi, &mut cs, &mut dc, 0x4E, &[0x00]);
        cmd(&mut spi, &mut cs, &mut dc, 0x4F, &[0x00, 0x00]);
        let _ = OutputPin::set_low(&mut dc);
        let _ = OutputPin::set_low(&mut cs);
        let _ = SpiBus::write(&mut spi, &[0x24]);
        let _ = OutputPin::set_high(&mut dc);
        for chunk in fb.chunks(64) {
            let _ = SpiBus::write(&mut spi, chunk);
        }
        let _ = OutputPin::set_high(&mut cs);

        cmd(&mut spi, &mut cs, &mut dc, 0x4E, &[0x00]);
        cmd(&mut spi, &mut cs, &mut dc, 0x4F, &[0x00, 0x00]);
        let _ = OutputPin::set_low(&mut dc);
        let _ = OutputPin::set_low(&mut cs);
        let _ = SpiBus::write(&mut spi, &[0x26]);
        let _ = OutputPin::set_high(&mut dc);
        for _ in 0..(FB_BYTES / 64) {
            let _ = SpiBus::write(&mut spi, &[0u8; 64]);
        }
        let _ = OutputPin::set_high(&mut cs);

        // Full refresh (0x22[0xF7] + 0x20).
        cmd(&mut spi, &mut cs, &mut dc, 0x22, &[0xF7]);
        cmd(&mut spi, &mut cs, &mut dc, 0x20, &[]);
        delay_ms(5_000);
        wait_ready(&mut busy);
        fill = !fill; // toggle 0x00 ↔ 0xFF
    }
}
