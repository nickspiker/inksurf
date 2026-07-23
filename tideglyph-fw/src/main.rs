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

    // Red diagnostic LED (P0.26 — the one that visibly blinked 2 Hz in the first blink test). Blinked 3× at the top of each loop iteration so we can tell if the SPI sequence is actually executing each cycle.
    let mut led = port0.p0_26.into_push_pull_output(Level::High).degrade();

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
    cmd(&mut spi, &mut cs, &mut dc, 0x3C, &[0x05]); // border
    cmd(&mut spi, &mut cs, &mut dc, 0x18, &[0x80]); // temperature sensor
    cmd(&mut spi, &mut cs, &mut dc, 0x4E, &[0x00]); // RAM X address counter
    cmd(&mut spi, &mut cs, &mut dc, 0x4F, &[0x00, 0x00]); // RAM Y address counter
    wait_ready(&mut busy);

    // DIAGNOSTIC: flip the whole panel black ↔ white forever. If the panel is
    // driven correctly you'll SEE it alternate; if it stays solid white, the
    // refresh isn't reaching it. This is unambiguous vs a panel that ships white.
    let fb = unsafe { &mut *core::ptr::addr_of_mut!(FB) };
    let mut fill: u8 = 0x00;
    loop {
        // 3 quick red blinks: proof the loop (SPI sequence) is executing.
        for _ in 0..3 {
            let _ = led.set_low();
            delay_ms(120);
            let _ = led.set_high();
            delay_ms(120);
        }
        for b in fb.iter_mut() {
            *b = fill;
        }
        // Reset the RAM address counter to (0,0) before each write.
        cmd(&mut spi, &mut cs, &mut dc, 0x4E, &[0x00]);
        cmd(&mut spi, &mut cs, &mut dc, 0x4F, &[0x00, 0x00]);
        // Write B/W RAM (0x24), chunked.
        let _ = OutputPin::set_low(&mut dc);
        let _ = OutputPin::set_low(&mut cs);
        let _ = SpiBus::write(&mut spi, &[0x24]);
        let _ = OutputPin::set_high(&mut dc);
        for chunk in fb.chunks(64) {
            let _ = SpiBus::write(&mut spi, chunk);
        }
        let _ = OutputPin::set_high(&mut cs);
        // Full refresh.
        cmd(&mut spi, &mut cs, &mut dc, 0x22, &[0xF7]);
        cmd(&mut spi, &mut cs, &mut dc, 0x20, &[]);
        delay_ms(5_000);
        wait_ready(&mut busy);
        fill = !fill; // toggle 0x00 ↔ 0xFF
    }
}
