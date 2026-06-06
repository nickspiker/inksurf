//! Adafruit 6383 / SSD1680Z 2.13" 250×122 BW grayscale panel.
//!
//! Native RAM layout: 122 columns × 250 gates. 122 px ÷ 8 = 15.25 → 16 bytes/row.
//! Multi-pass grayscale via custom LUT writes — see host-side recipe table.

use embassy_rp::gpio::{Input, Output};
use embassy_rp::peripherals::{SPI0, USB};
use embassy_rp::spi::{Async, Spi};
use embassy_rp::usb::Driver as UsbDriver;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{Receiver, Sender};
use static_cell::StaticCell;

use crate::{RxStream, say, say_hex_byte};

const PANEL_W: usize = 122;
const PANEL_H: usize = 250;
const ROW_BYTES: usize = (PANEL_W + 7) / 8; // 16
const FB_BYTES: usize = ROW_BYTES * PANEL_H; // 4000

const LUT_LEN: usize = 159; // 153 LUT bytes + 6 voltage/timing tail bytes

pub const USB_PRODUCT: &str = "eink-ssd1680";

const TAIL_DEFAULT: [u8; 6] = [0x22, 0x17, 0x41, 0x00, 0x32, 0x36];

struct Ssd1680 {
    spi: Spi<'static, SPI0, Async>,
    cs: Output<'static>,
    dc: Output<'static>,
    rst: Output<'static>,
    busy: Input<'static>,
}

impl Ssd1680 {
    async fn wait_idle(&mut self) {
        while self.busy.is_high() {
            Timer::after_millis(1).await;
        }
    }

    async fn hw_reset(&mut self) {
        self.rst.set_high();
        Timer::after_millis(10).await;
        self.rst.set_low();
        Timer::after_millis(10).await;
        self.rst.set_high();
        Timer::after_millis(10).await;
        self.wait_idle().await;
    }

    async fn cmd(&mut self, c: u8) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[c]).await;
        self.cs.set_high();
    }

    async fn data(&mut self, d: &[u8]) {
        self.dc.set_high();
        self.cs.set_low();
        let _ = self.spi.write(d).await;
        self.cs.set_high();
    }

    async fn cmd_data(&mut self, c: u8, d: &[u8]) {
        self.cmd(c).await;
        self.data(d).await;
    }

    async fn init(&mut self) {
        self.hw_reset().await;

        self.cmd(0x12).await; // soft reset
        self.wait_idle().await;

        let g = (PANEL_H - 1) as u16;
        self.cmd_data(0x01, &[(g & 0xff) as u8, (g >> 8) as u8, 0x00])
            .await;
        self.cmd_data(0x11, &[0x03]).await; // X inc, Y inc
        self.cmd_data(0x44, &[0x00, (ROW_BYTES - 1) as u8]).await;
        self.cmd_data(0x45, &[0x00, 0x00, (g & 0xff) as u8, (g >> 8) as u8]).await;
        self.cmd_data(0x3C, &[0x05]).await; // border = white
        self.cmd_data(0x21, &[0x00, 0x80]).await;
        self.cmd_data(0x18, &[0x80]).await; // internal temp sensor
        self.cmd_data(0x4E, &[0x00]).await;
        self.cmd_data(0x4F, &[0x00, 0x00]).await;
        self.wait_idle().await;
    }

    async fn write_bw(&mut self, fb: &[u8]) {
        self.cmd_data(0x4E, &[0x00]).await;
        self.cmd_data(0x4F, &[0x00, 0x00]).await;
        self.cmd(0x24).await;
        self.data(fb).await;
    }

    async fn fill_bw(&mut self, byte: u8) {
        self.cmd_data(0x4E, &[0x00]).await;
        self.cmd_data(0x4F, &[0x00, 0x00]).await;
        self.cmd(0x24).await;
        self.dc.set_high();
        self.cs.set_low();
        let chunk = [byte; 64];
        let mut sent = 0;
        while sent < FB_BYTES {
            let n = (FB_BYTES - sent).min(64);
            let _ = self.spi.write(&chunk[..n]).await;
            sent += n;
        }
        self.cs.set_high();
    }

    async fn write_red_blank(&mut self) {
        self.cmd_data(0x4E, &[0x00]).await;
        self.cmd_data(0x4F, &[0x00, 0x00]).await;
        self.cmd(0x26).await;
        self.dc.set_high();
        self.cs.set_low();
        let zeros = [0u8; 64];
        let mut sent = 0;
        while sent < FB_BYTES {
            let n = (FB_BYTES - sent).min(zeros.len());
            let _ = self.spi.write(&zeros[..n]).await;
            sent += n;
        }
        self.cs.set_high();
    }

    async fn write_prev(&mut self, fb: &[u8]) {
        self.cmd_data(0x4E, &[0x00]).await;
        self.cmd_data(0x4F, &[0x00, 0x00]).await;
        self.cmd(0x26).await;
        self.data(fb).await;
    }

    async fn refresh_full(&mut self) {
        self.cmd_data(0x22, &[0xF7]).await;
        self.cmd(0x20).await;
        self.wait_idle().await;
    }

    async fn upload_lut(&mut self, lut: &[u8; LUT_LEN]) {
        self.cmd_data(0x32, &lut[0..153]).await;
        self.cmd_data(0x3F, &lut[153..154]).await;
        self.cmd_data(0x03, &lut[154..155]).await;
        self.cmd_data(0x04, &lut[155..158]).await;
        self.cmd_data(0x2C, &lut[158..159]).await;
    }

    async fn refresh_with_loaded_lut(&mut self) {
        self.cmd_data(0x22, &[0xCF]).await;
        self.cmd(0x20).await;
        self.wait_idle().await;
    }
}

fn build_pulse_lut(out: &mut [u8; LUT_LEN], voltage: u8, frames: u8) {
    out.fill(0);
    out[0] = voltage;
    out[12] = voltage;
    out[24] = voltage;
    out[36] = voltage;
    out[60] = frames;
    out[144] = 1;
    out[153..159].copy_from_slice(&TAIL_DEFAULT);
}

fn fill_stripes(fb: &mut [u8]) {
    for y in 0..PANEL_H {
        let row = &mut fb[y * ROW_BYTES..(y + 1) * ROW_BYTES];
        let band = (y / 8) & 1;
        let byte = if band == 0 { 0xFFu8 } else { 0x00u8 };
        for b in row.iter_mut() {
            *b = byte;
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
    let mut panel = Ssd1680 { spi, cs, dc, rst, busy };

    static FB: StaticCell<[u8; FB_BYTES]> = StaticCell::new();
    let fb = FB.init([0xFF; FB_BYTES]);

    static PREV_FB: StaticCell<[u8; FB_BYTES]> = StaticCell::new();
    let prev_fb = PREV_FB.init([0xFF; FB_BYTES]);

    static PULSE_LUT: StaticCell<[u8; LUT_LEN]> = StaticCell::new();
    let pulse_lut = PULSE_LUT.init([0; LUT_LEN]);

    static USER_LUT: StaticCell<[u8; LUT_LEN]> = StaticCell::new();
    let user_lut = USER_LUT.init([0; LUT_LEN]);

    panel.init().await;

    say(
        &mut sender,
        b"\r\nferros eink-ssd1680 v0.2\r\n\
        Commands:\r\n\
        'T' = stripe test\r\n\
        'I' + 4000B = upload + refresh BW image\r\n\
        'W' = clear to white (OTP refresh)\r\n\
        'K' = clear to black (OTP refresh)\r\n\
        'F' + 1B = fill BW plane with byte, no refresh\r\n\
        'P' + voltage:1B + frames:1B = 1-phase pulse LUT and refresh\r\n\
        'L' + 159B = upload arbitrary LUT and refresh\r\n\
        'A' + 4000B(prev) + 4000B(new) + 159B(LUT) = multi-pass step\r\n\
        'B' = sample BUSY pin\r\n\
        \r\n[boot] writing stripe pattern...\r\n",
    )
    .await;

    fill_stripes(fb);
    panel.write_bw(fb).await;
    panel.write_red_blank().await;
    panel.refresh_full().await;
    say(&mut sender, b"[boot] done\r\n").await;

    let mut rs = RxStream::new(&mut receiver);
    loop {
        let cmd = rs.read_byte().await;
        match cmd {
            b'T' | b't' => {
                say(&mut sender, b"[T] stripe pattern\r\n").await;
                fill_stripes(fb);
                panel.write_bw(fb).await;
                panel.write_red_blank().await;
                panel.refresh_full().await;
                say(&mut sender, b"[T] done\r\n").await;
            }
            b'I' | b'i' => {
                say(&mut sender, b"[I] expecting 4000B BW...\r\n").await;
                if rs.read_exact(fb).await {
                    panel.write_bw(fb).await;
                    panel.write_red_blank().await;
                    panel.refresh_full().await;
                    say(&mut sender, b"[I] refreshed\r\n").await;
                } else {
                    say(&mut sender, b"[I] timeout\r\n").await;
                }
            }
            b'W' | b'w' => {
                say(&mut sender, b"[W] clear to white\r\n").await;
                panel.init().await;
                panel.fill_bw(0xFF).await;
                panel.write_red_blank().await;
                panel.refresh_full().await;
                say(&mut sender, b"[W] done\r\n").await;
            }
            b'K' | b'k' => {
                say(&mut sender, b"[K] clear to black\r\n").await;
                panel.init().await;
                panel.fill_bw(0x00).await;
                panel.write_red_blank().await;
                panel.refresh_full().await;
                say(&mut sender, b"[K] done\r\n").await;
            }
            b'F' | b'f' => {
                let mut arg = [0u8; 1];
                if !rs.read_exact(&mut arg).await {
                    say(&mut sender, b"[F] timeout\r\n").await;
                    continue;
                }
                say(&mut sender, b"[F] fill BW\r\n").await;
                panel.fill_bw(arg[0]).await;
                panel.write_red_blank().await;
                say(&mut sender, b"[F] done\r\n").await;
            }
            b'P' | b'p' => {
                let mut arg = [0u8; 2];
                if !rs.read_exact(&mut arg).await {
                    say(&mut sender, b"[P] timeout\r\n").await;
                    continue;
                }
                let (voltage, frames) = (arg[0], arg[1]);
                say(&mut sender, b"[P] pulse voltage=").await;
                say_hex_byte(&mut sender, voltage).await;
                say(&mut sender, b" frames=").await;
                say_hex_byte(&mut sender, frames).await;
                say(&mut sender, b"\r\n").await;
                build_pulse_lut(pulse_lut, voltage, frames);
                panel.upload_lut(pulse_lut).await;
                panel.refresh_with_loaded_lut().await;
                say(&mut sender, b"[P] done\r\n").await;
            }
            b'L' | b'l' => {
                say(&mut sender, b"[L] expecting 159B LUT...\r\n").await;
                if rs.read_exact(user_lut).await {
                    panel.upload_lut(user_lut).await;
                    panel.refresh_with_loaded_lut().await;
                    say(&mut sender, b"[L] done\r\n").await;
                } else {
                    say(&mut sender, b"[L] timeout\r\n").await;
                }
            }
            b'A' | b'a' => {
                say(&mut sender, b"[A] expecting 4000B prev + 4000B new + 159B LUT...\r\n").await;
                if !rs.read_exact(prev_fb).await {
                    say(&mut sender, b"[A] prev timeout\r\n").await;
                    continue;
                }
                if !rs.read_exact(fb).await {
                    say(&mut sender, b"[A] new timeout\r\n").await;
                    continue;
                }
                if !rs.read_exact(user_lut).await {
                    say(&mut sender, b"[A] LUT timeout\r\n").await;
                    continue;
                }
                panel.write_bw(fb).await;
                panel.write_prev(prev_fb).await;
                panel.upload_lut(user_lut).await;
                panel.refresh_with_loaded_lut().await;
                say(&mut sender, b"[A] done\r\n").await;
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
