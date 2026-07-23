//! P0 panel bring-up: drive the Seeed 1.54" **BWRY** panel (JD79660, 200×200, 4-color black/white/yellow/red) via the Seeed nRF52840 + EN04/EN05 board, and push a 4-color band test frame.
//!
//! Config from Seeed's Setup517 (JD79660): 2 bits/pixel (4 px/byte), pixel write via DTM = 0x10, refresh = 0x12 + [0x00], BUSY LOW = busy (opposite of the SSD1681). Runs BARE at 0x1000 (no SoftDevice — see memory.x). Pins are the EN04/EN05 board's nRF GPIO. Diagnostic LED on D9 = P1.14.

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

// JD79660 BWRY: 200×200, 2 bits/pixel, 4 px/byte.
const W: usize = 200;
const H: usize = 200;
const ROW_BYTES: usize = W / 4; // 50
const FB_BYTES: usize = ROW_BYTES * H; // 10000

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

    // Init (Seeed Setup517 / JD79660, 200×200 BWRY).
    cmd(&mut spi, &mut cs, &mut dc, 0x4D, &[0x78]);
    cmd(&mut spi, &mut cs, &mut dc, 0x00, &[0x0F, 0x29]); // PSR
    cmd(&mut spi, &mut cs, &mut dc, 0x06, &[0x0D, 0x12, 0x30, 0x20, 0x19, 0x2A, 0x22]); // BTST_P
    cmd(&mut spi, &mut cs, &mut dc, 0x50, &[0x37]); // CDI
    cmd(&mut spi, &mut cs, &mut dc, 0x61, &[0x00, 0xC8, 0x00, 0xC8]); // TRES 200×200
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
