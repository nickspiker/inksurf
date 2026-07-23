//! P0 panel bring-up: drive the **Adafruit 6414 BWRY** panel (JD79667, 384×180 landscape = 180×384 chip coords, 4-color black/white/yellow/red) via the Seeed nRF52840 + EN04/EN05 board, and push a 4-color band test frame.
//!
//! Geometry + init from inksurf's proven src/panel_jd79667.rs: 2 bits/pixel (4 px/byte), TRES 180×384 → [0x00,0xB4,0x01,0x80], DTM = 0x10, refresh = 0x12 + [0x00], BUSY LOW = busy. Runs BARE at 0x1000 (no SoftDevice — see memory.x). Pins are the EN04/EN05 board's nRF GPIO. Diagnostic LED on D9 = P1.14.

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

// Diagnostic LED on D9 = P1.14 (unused MISO), driven by DIRECT registers.
const P1_BASE: u32 = 0x5000_0300;
const LED_DIRSET: u32 = P1_BASE + 0x518;
const LED_OUTSET: u32 = P1_BASE + 0x508;
const LED_OUTCLR: u32 = P1_BASE + 0x50C;
const LED_PIN: u32 = 14; // P1.14 = D9

#[inline(always)]
fn led_init() {
    unsafe { core::ptr::write_volatile(LED_DIRSET as *mut u32, 1 << LED_PIN) }
}
#[inline(always)]
fn led(on: bool) {
    let reg = if on { LED_OUTSET } else { LED_OUTCLR };
    unsafe { core::ptr::write_volatile(reg as *mut u32, 1 << LED_PIN) }
}

// Adafruit 6414 BWRY in chip coordinates: 180 wide × 384 tall, 2 bits/pixel.
const W: usize = 180;
const H: usize = 384;
const ROW_BYTES: usize = W / 4; // 45
const FB_BYTES: usize = 17_664; // 45×384 = 17,280 data + trailing padding (matches inksurf)

// 2-bit colour codes (JD79660 / inksurf), packed 4/byte.
const BLACK: u8 = 0b00;
const WHITE: u8 = 0b01;
const YELLOW: u8 = 0b10;
const RED: u8 = 0b11;

fn delay_ms(ms: u32) {
    cpu_delay(64_000 * ms);
}

/// JD79660 BUSY is LOW while busy, so wait until it goes HIGH. Bounded (~5 s).
fn wait_ready<P: InputPin>(busy: &mut P) {
    for _ in 0..5000 {
        if busy.is_high().unwrap_or(true) {
            return;
        }
        delay_ms(1);
    }
}

static mut FB: [u8; FB_BYTES] = [0; FB_BYTES];

#[entry]
fn main() -> ! {
    led_init();
    led(true);

    let p = hal::pac::Peripherals::take().unwrap();
    let port0 = p0::Parts::new(p.P0);
    let port1 = p1::Parts::new(p.P1);

    // SPI pins: SCLK=P1.13 (D8), MOSI=P1.15 (D10).
    let sck = port1.p1_13.into_push_pull_output(Level::Low).degrade();
    let mosi = port1.p1_15.into_push_pull_output(Level::Low).degrade();
    let mut spi = Spim::new(
        p.SPIM0,
        Pins { sck: Some(sck), mosi: Some(mosi), miso: None },
        Frequency::M8,
        MODE_0,
        0,
    );

    // Control pins: CS=D7=P1.12, DC=D16=P0.31, RST=D11=P0.15, BUSY=D3=P0.29, EN=D6=P1.11.
    let mut panel_en = port1.p1_11.into_push_pull_output(Level::Low).degrade();
    let mut cs = port1.p1_12.into_push_pull_output(Level::High).degrade();
    let mut dc = port0.p0_31.into_push_pull_output(Level::Low).degrade();
    let mut rst = port0.p0_15.into_push_pull_output(Level::High).degrade();
    let mut busy = port0.p0_29.into_floating_input().degrade();

    // Power, settle, reset (JD79660 timing: 50 ms low / 100 ms high).
    let _ = panel_en.set_high();
    delay_ms(10);
    let _ = rst.set_low();
    delay_ms(50);
    let _ = rst.set_high();
    delay_ms(100);
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

    // Init (inksurf's proven JD79667 sequence for the Adafruit 6414, 180×384).
    cmd(&mut spi, &mut cs, &mut dc, 0x4D, &[0x78]);
    cmd(&mut spi, &mut cs, &mut dc, 0x00, &[0x0F, 0x29]); // PSR
    cmd(&mut spi, &mut cs, &mut dc, 0x01, &[0x07, 0x00]); // PWRR
    cmd(&mut spi, &mut cs, &mut dc, 0x03, &[0x10, 0x54, 0x44]); // POFS
    cmd(&mut spi, &mut cs, &mut dc, 0x06, &[0x05, 0x00, 0x3F, 0x0A, 0x25, 0x12, 0x1A]); // BTST
    cmd(&mut spi, &mut cs, &mut dc, 0x50, &[0x37]); // CDI
    cmd(&mut spi, &mut cs, &mut dc, 0x60, &[0x02, 0x02]); // TCON
    cmd(&mut spi, &mut cs, &mut dc, 0x61, &[0x00, 0xB4, 0x01, 0x80]); // TRES 180×384
    cmd(&mut spi, &mut cs, &mut dc, 0xE7, &[0x1C]);
    cmd(&mut spi, &mut cs, &mut dc, 0xE3, &[0x22]);
    cmd(&mut spi, &mut cs, &mut dc, 0xB4, &[0xD0]);
    cmd(&mut spi, &mut cs, &mut dc, 0xB5, &[0x03]);
    cmd(&mut spi, &mut cs, &mut dc, 0xE9, &[0x01]);
    cmd(&mut spi, &mut cs, &mut dc, 0x30, &[0x08]); // PLL
    cmd(&mut spi, &mut cs, &mut dc, 0x04, &[]); // POWER ON
    wait_ready(&mut busy);

    // Build a 4-colour band test frame: black / white / yellow / red, top to bottom.
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
    for &b in fb.iter() {
        let _ = SpiBus::write(&mut spi, &[b]);
    }
    let _ = OutputPin::set_high(&mut cs);

    cmd(&mut spi, &mut cs, &mut dc, 0x12, &[0x00]); // DRF refresh
    delay_ms(2_000);
    wait_ready(&mut busy);

    // Done — heartbeat the LED, leave the image up.
    loop {
        led(true);
        delay_ms(150);
        led(false);
        delay_ms(1_850);
    }
}
