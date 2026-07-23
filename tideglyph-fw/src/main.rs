//! P0 panel bring-up: drive the **Adafruit 6414** BWRY panel (JD79667, 384×180 landscape; 180×384 in chip coords) via the Seeed nRF52840 + EN04 board, and push a 4-band test frame (black / white / yellow / red).
//!
//! The panel currently wired to the board is the Adafruit 6414 (the one that already shows the old tide chart), not the Seeed 2.9". Its driver is inksurf's proven src/panel_jd79667.rs: identical JD79667 init EXCEPT TRES geometry (180×384 → [0x00,0xB4,0x01,0x80]), DTM=0x10, DRF=0x12+[0x00], BUSY low=busy. Pins remain the Seeed EN04 board's nRF GPIO (panel connects through it).
//!
//! Caveat: the Adafruit ribbon's pin assignments may not match the Seeed FPC connector; if this shows nothing, that mismatch is the likely hardware cause.

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

// Adafruit 6414 in chip coordinates: 180 wide × 384 tall, 2 bits/px.
const W: usize = 180;
const H: usize = 384;
const ROW_BYTES: usize = W / 4; // 45
// inksurf's firmware streams 17,664 B (45×384 = 17,280 data + trailing padding).
const FB_BYTES: usize = 17_664;

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

    // Diagnostic LEDs on the RGB (active-low), chosen to be unmistakable vs the
    // board's hardware orange charge LED: BLUE = "my code is running", GREEN =
    // "panel sequence completed". Boot proof: 3 deliberate blue pulses up front —
    // if you see those, the app is definitely executing (not the charge LED).
    let mut blue = port0.p0_06.into_push_pull_output(Level::High).degrade();
    let mut green = port0.p0_30.into_push_pull_output(Level::High).degrade();
    for _ in 0..3 {
        let _ = blue.set_low();
        delay_ms(300);
        let _ = blue.set_high();
        delay_ms(300);
    }
    let _ = blue.set_low(); // BLUE stays on through the panel sequence

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
    cmd(&mut spi, &mut cs, &mut dc, 0x61, &[0x00, 0xB4, 0x01, 0x80]); // TRES 180×384 (Adafruit 6414)
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

    // Stream the framebuffer via DTM = 0x10 (Adafruit 6414 / inksurf), then
    // refresh (DRF = 0x12 + [0x00]).
    let _ = OutputPin::set_low(&mut dc);
    let _ = OutputPin::set_low(&mut cs);
    let _ = SpiBus::write(&mut spi, &[0x10]);
    let _ = OutputPin::set_high(&mut dc);
    let _ = SpiBus::write(&mut spi, &fb[..]);
    let _ = OutputPin::set_high(&mut cs);

    cmd(&mut spi, &mut cs, &mut dc, 0x12, &[0x00]); // DRF refresh
    // Unconditional long wait — do NOT trust BUSY (if it's mis-wired it reads
    // high and we'd power off mid-refresh, blanking the panel). A BWRY full
    // refresh is several seconds; give it 25 s, then leave the panel powered.
    delay_ms(25_000);
    // (No power-off — keep it simple; the image is latched by the refresh.)

    // Completed the full sequence: blue off, green heartbeat. Blue-stuck-on (no
    // green) means it hung mid-init; green blinking means everything finished.
    let _ = blue.set_high();
    loop {
        let _ = green.set_low();
        delay_ms(150);
        let _ = green.set_high();
        delay_ms(850);
    }
}
