//! Adafruit 6414 / JD79667 3.52" 340×180 quad-color BWRY panel.
//!
//! Each pixel is one of {Black, White, Yellow, Red}, encoded 2 bits per pixel:
//!   0b00 = Black, 0b01 = White, 0b10 = Yellow, 0b11 = Red.
//! 4 pixels per byte, MSB first: byte = (p0<<6)|(p1<<4)|(p2<<2)|p3.
//!
//! Width is padded up to a multiple of 8 (340 → 344) before computing buffer size,
//! matching what Adafruit_EPD does. Total framebuffer: 344 × 180 / 4 = 15,480 B.
//!
//! Init sequence ported from Adafruit_JD79667.cpp.

use embassy_rp::gpio::{Input, Output};
use embassy_rp::peripherals::{SPI0, USB};
use embassy_rp::spi::{Async, Spi};
use embassy_rp::usb::Driver as UsbDriver;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{Receiver, Sender};
use static_cell::StaticCell;

use crate::{RxStream, say};

// Chip-native orientation per Adafruit_JD79667 wrapper:
// ThinkInk_352_Quadcolor_AJHE5 passes WIDTH=180, HEIGHT=384 to constructor.
// Physical panel is 340 wide × 180 tall landscape, but the chip rasterizes
// as 180-wide rows × 384 chip-rows. Visible area = chip-rows 0..339;
// chip-rows 340..383 are off-panel padding. Adafruit's setRotation(1) handles
// the user-facing landscape mapping in software.
const CHIP_W: usize = 180;
const CHIP_H: usize = 384;
const ROW_BYTES: usize = CHIP_W / 4; // 45
// Buffer = width rounded to mult of 8 (184) × height / 4 = 17,664.
const FB_BYTES: usize = ((CHIP_W + 7) & !7) * CHIP_H / 4; // 17,664

// Command codes
const CMD_PSR: u8 = 0x00; // Panel Setting
const CMD_PWR: u8 = 0x01; // Power Setting
const CMD_POW: u8 = 0x04; // Power On
const CMD_BTST: u8 = 0x06; // Booster Soft Start
const CMD_DTM: u8 = 0x10; // Data Start Transmission
const CMD_DRF: u8 = 0x12; // Display Refresh
const CMD_PLL: u8 = 0x30; // PLL Control
const CMD_CDI: u8 = 0x50; // VCOM / Data Interval

struct Jd79667 {
    spi: Spi<'static, SPI0, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    rst: Output<'static>,
    busy: Input<'static>,
}

impl Jd79667 {
    async fn wait_idle(&mut self) {
        // JD79667 polarity is INVERTED vs SSD1680. Per Adafruit_JD79667::busy_wait:
        //   while (!digitalRead(_busy_pin)) // wait for busy HIGH!
        // BUSY LOW = chip busy, BUSY HIGH = chip idle/ready.
        // Capped at 30s so a missing/floating BUSY signal can't deadlock init.
        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_secs(30);
        while self.busy.is_low() && embassy_time::Instant::now() < deadline {
            Timer::after_millis(1).await;
        }
    }

    async fn hw_reset(&mut self) {
        // Timings from Adafruit_JD79667::hardwareReset (20/40/50 ms).
        self.rst.set_high();
        Timer::after_millis(20).await;
        self.rst.set_low();
        Timer::after_millis(40).await;
        self.rst.set_high();
        Timer::after_millis(50).await;
        self.wait_idle().await;
    }

    async fn cmd(&mut self, c: u8) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[c]).await;
        self.cs.set_high();
    }

    /// Standard EPD transaction: hold CS low through both cmd and data phases,
    /// toggling DC to demarcate them.
    async fn cmd_data(&mut self, c: u8, d: &[u8]) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[c]).await;
        self.dc.set_high();
        let _ = self.spi.write(d).await;
        self.cs.set_high();
    }

    /// Init sequence from Adafruit_JD79667.cpp `jd79667_default_init_code`.
    async fn init(&mut self) {
        self.hw_reset().await;
        Timer::after_millis(10).await;

        self.cmd_data(0x4D, &[0x78]).await;
        self.cmd_data(CMD_PSR, &[0x0F, 0x29]).await;
        self.cmd_data(CMD_PWR, &[0x07, 0x00]).await;
        self.cmd_data(0x03, &[0x10, 0x54, 0x44]).await; // POFS
        self.cmd_data(CMD_BTST, &[0x05, 0x00, 0x3F, 0x0A, 0x25, 0x12, 0x1A]).await;
        self.cmd_data(CMD_CDI, &[0x37]).await;
        self.cmd_data(0x60, &[0x02, 0x02]).await; // TCON
        // TRES: 0x00B4 = 180 wide × 0x0180 = 384 tall (chip-internal coord)
        self.cmd_data(0x61, &[0x00, 0xB4, 0x01, 0x80]).await;
        self.cmd_data(0xE7, &[0x1C]).await;
        self.cmd_data(0xE3, &[0x22]).await;
        self.cmd_data(0xB4, &[0xD0]).await;
        self.cmd_data(0xB5, &[0x03]).await;
        self.cmd_data(0xE9, &[0x01]).await;
        self.cmd_data(CMD_PLL, &[0x08]).await;
        self.cmd(CMD_POW).await;
        self.wait_idle().await;
    }

    /// Stream a single 2bpp framebuffer (15,480 B) to the chip via DTM.
    /// CS stays low through cmd + all data.
    async fn write_image(&mut self, fb: &[u8]) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[CMD_DTM]).await;
        self.dc.set_high();
        let _ = self.spi.write(fb).await;
        self.cs.set_high();
    }

    /// Fill the entire framebuffer with one color code repeated across all
    /// 4 pixel slots of every byte. No host buffer needed.
    async fn fill_color(&mut self, color: u8) {
        let packed = pack_byte(color, color, color, color);
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[CMD_DTM]).await;
        self.dc.set_high();
        let chunk = [packed; 64];
        let mut sent = 0;
        while sent < FB_BYTES {
            let n = (FB_BYTES - sent).min(64);
            let _ = self.spi.write(&chunk[..n]).await;
            sent += n;
        }
        self.cs.set_high();
    }

    async fn refresh(&mut self) {
        self.cmd_data(CMD_DRF, &[0x00]).await;
        self.wait_idle().await;
        // Adafruit pattern uses an additional 13s settling delay after busy_wait
        // returns, before powering down. We don't know if BUSY actually wired
        // through; this extra fixed delay keeps the chip undisturbed long enough
        // for the ink to fully settle.
        Timer::after_millis(13_000).await;
    }

    /// Soft power-off only (no deep sleep). Chip stays addressable but the
    /// HV booster shuts down, conserving power between refreshes. Next refresh
    /// just needs power-on, not a full hw_reset.
    async fn power_off(&mut self) {
        self.cmd(0x02).await; // POWER_OFF
        Timer::after_millis(100).await;
    }

    async fn power_on(&mut self) {
        self.cmd(0x04).await; // POWER_ON
        self.wait_idle().await;
    }
}

/// Pack four 2-bit color codes (0..3) into one byte, MSB-first.
fn pack_byte(p0: u8, p1: u8, p2: u8, p3: u8) -> u8 {
    ((p0 & 0x3) << 6) | ((p1 & 0x3) << 4) | ((p2 & 0x3) << 2) | (p3 & 0x3)
}

/// Boot test pattern: vertical bands of B/W/Y/R viewed landscape.
/// In chip orientation those are horizontal bands across chip-rows
/// (chip-row maps to physical landscape-X). All 384 chip-rows are visible
/// on this panel — Adafruit's 340 marketing dimension underreports the
/// active area by 44 px.
fn fill_color_bands(fb: &mut [u8]) {
    fb.fill(0x55);
    const VISIBLE_ROWS: usize = 384;
    for chip_row in 0..VISIBLE_ROWS {
        let band = chip_row * 4 / VISIBLE_ROWS;
        let color: u8 = match band {
            0 => 0b00, // black
            1 => 0b01, // white
            2 => 0b10, // yellow
            _ => 0b11, // red
        };
        let packed = pack_byte(color, color, color, color);
        for col_byte in 0..ROW_BYTES {
            fb[chip_row * ROW_BYTES + col_byte] = packed;
        }
    }
}

pub async fn run(
    spi: Spi<'static, SPI0, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    rst: Output<'static>,
    busy: Input<'static>,
    mut sender: Sender<'static, UsbDriver<'static, USB>>,
    mut receiver: Receiver<'static, UsbDriver<'static, USB>>,
) -> ! {
    let mut panel = Jd79667 { spi, cs, dc, rst, busy };

    // Zero-init via BSS, then fill at runtime — avoids a 17,664-byte stack
    // temporary inside the spawned task during StaticCell::init.
    static FB: StaticCell<[u8; FB_BYTES]> = StaticCell::new();
    let fb = FB.init([0u8; FB_BYTES]);
    fb.fill(0x55); // 0x55 = WWWW packed (all white)

    say(
        &mut sender,
        b"\r\nferros eink-jd79667 v0.2\r\n\
        Commands:\r\n\
        'T' = BWYR bands test\r\n\
        'I' + 17664B = upload + refresh 2bpp BWRY image\r\n\
        'W' = fill white + refresh\r\n\
        'K' = fill black + refresh\r\n\
        'Y' = fill yellow + refresh\r\n\
        'R' = fill red + refresh\r\n\
        'B' = sample BUSY pin\r\n\
        \r\n[boot] init chip, waiting for I command...\r\n",
    )
    .await;

    // Boot init wakes the chip from any prior deep-sleep state and ends with POWER_ON; we immediately power off and wait for a host I command before refreshing. No boot test pattern - whatever was last on the panel stays put.
    panel.init().await;
    panel.power_off().await;
    say(&mut sender, b"[boot] ready\r\n").await;

    let mut rs = RxStream::new(&mut receiver);
    loop {
        let cmd = rs.read_byte().await;
        match cmd {
            b'T' | b't' => {
                say(&mut sender, b"[T] color bands\r\n").await;
                fill_color_bands(fb);
                panel.power_on().await;
                panel.write_image(fb).await;
                panel.refresh().await;
                panel.power_off().await;
                say(&mut sender, b"[T] done\r\n").await;
            }
            b'I' | b'i' => {
                say(&mut sender, b"[I] expecting 17664B BWRY...\r\n").await;
                if rs.read_exact(fb).await {
                    panel.power_on().await;
                    panel.write_image(fb).await;
                    panel.refresh().await;
                    panel.power_off().await;
                    say(&mut sender, b"[I] refreshed\r\n").await;
                } else {
                    say(&mut sender, b"[I] timeout\r\n").await;
                }
            }
            b'W' | b'w' => {
                say(&mut sender, b"[W] fill white\r\n").await;
                panel.power_on().await;
                panel.fill_color(0b01).await;
                panel.refresh().await;
                panel.power_off().await;
                say(&mut sender, b"[W] done\r\n").await;
            }
            b'K' | b'k' => {
                say(&mut sender, b"[K] fill black\r\n").await;
                panel.power_on().await;
                panel.fill_color(0b00).await;
                panel.refresh().await;
                panel.power_off().await;
                say(&mut sender, b"[K] done\r\n").await;
            }
            b'Y' | b'y' => {
                say(&mut sender, b"[Y] fill yellow\r\n").await;
                panel.power_on().await;
                panel.fill_color(0b10).await;
                panel.refresh().await;
                panel.power_off().await;
                say(&mut sender, b"[Y] done\r\n").await;
            }
            b'R' | b'r' => {
                say(&mut sender, b"[R] fill red\r\n").await;
                panel.power_on().await;
                panel.fill_color(0b11).await;
                panel.refresh().await;
                panel.power_off().await;
                say(&mut sender, b"[R] done\r\n").await;
            }
            b'B' | b'b' => {
                let v = if panel.busy.is_high() { b'1' } else { b'0' };
                say(&mut sender, b"[B] busy=").await;
                let _ = sender.write_packet(&[v]).await;
                say(&mut sender, b"\r\n").await;
            }
            _ => {}
        }
    }
}
