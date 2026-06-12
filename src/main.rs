#![no_std]
#![no_main]

use cortex_m_rt as _;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::pac;
use embassy_rp::peripherals::USB;
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_rp::usb::{Driver as UsbDriver, InterruptHandler as UsbInterruptHandler};
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::driver::Driver as UsbDriverTrait;
use embassy_usb::{Builder, Config as UsbConfig};
use panic_halt as _;
use static_cell::StaticCell;

mod panel_ssd1680;
mod panel_jd79667;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

// ============================================================================
// Geiger heartbeat on GP29 (inverted-polarity UV LED).
// ============================================================================
const GALOIS_TAPS: u32 = 0x80200003;
const OFF_ANDS: u32 = 7;

fn lfsr_step(state: &mut u32) {
    let trng = pac::ROSC.randombit().read().randombit() as u32;
    *state ^= trng;
    let lsb = *state & 1;
    *state >>= 1;
    if lsb != 0 {
        *state ^= GALOIS_TAPS;
    }
}

fn on_pulse_us(state: &mut u32) -> u64 {
    lfsr_step(state);
    1 + state.leading_zeros() as u64
}

fn and_chunk(state: &mut u32, ands: u32) -> u32 {
    let mut chunk: u32 = u32::MAX;
    for _ in 0..ands {
        lfsr_step(state);
        chunk &= *state;
        if chunk == 0 {
            return 0;
        }
    }
    chunk
}

fn off_pulse_us(state: &mut u32) -> u64 {
    let mut total_zeros: u64 = 0;
    loop {
        let chunk = and_chunk(state, OFF_ANDS);
        if chunk != 0 {
            return total_zeros + chunk.leading_zeros() as u64 + 1;
        }
        total_zeros += 32;
    }
}

#[embassy_executor::task]
async fn geiger_task(mut led: Output<'static>) {
    let mut state: u32 = 0;
    for _ in 0..32 {
        state = (state << 1) | (pac::ROSC.randombit().read().randombit() as u32);
    }
    if state == 0 {
        state = 0xACE1DEED;
    }
    // Feather D13 LED: HIGH = on, LOW = off.
    loop {
        let on_us = on_pulse_us(&mut state);
        led.set_high();
        Timer::after_micros(on_us).await;
        let off_us = off_pulse_us(&mut state);
        led.set_low();
        Timer::after_micros(off_us).await;
    }
}

// ============================================================================
// USB plumbing + RX byte stream — shared between panels.
// ============================================================================

#[embassy_executor::task]
async fn run_usb(mut device: embassy_usb::UsbDevice<'static, UsbDriver<'static, USB>>) {
    device.run().await;
}

pub async fn say<'d, D: UsbDriverTrait<'d>>(s: &mut Sender<'d, D>, msg: &[u8]) {
    for chunk in msg.chunks(64) {
        let _ = s.write_packet(chunk).await;
    }
}

pub async fn say_hex_byte<'d, D: UsbDriverTrait<'d>>(s: &mut Sender<'d, D>, v: u8) {
    fn nib(n: u8) -> u8 {
        if n < 10 { b'0' + n } else { b'a' + n - 10 }
    }
    let buf = [b'0', b'x', nib(v >> 4), nib(v & 0xf)];
    let _ = s.write_packet(&buf).await;
}

pub struct RxStream<'a, 'd, D: UsbDriverTrait<'d>> {
    rx: &'a mut Receiver<'d, D>,
    buf: [u8; 64],
    len: usize,
    pos: usize,
}

impl<'a, 'd, D: UsbDriverTrait<'d>> RxStream<'a, 'd, D> {
    pub fn new(rx: &'a mut Receiver<'d, D>) -> Self {
        Self { rx, buf: [0; 64], len: 0, pos: 0 }
    }

    pub async fn read_byte(&mut self) -> u8 {
        loop {
            if self.pos < self.len {
                let b = self.buf[self.pos];
                self.pos += 1;
                return b;
            }
            match self.rx.read_packet(&mut self.buf).await {
                Ok(n) if n > 0 => {
                    self.len = n;
                    self.pos = 0;
                }
                _ => {
                    self.len = 0;
                    self.pos = 0;
                }
            }
        }
    }

    pub async fn read_exact(&mut self, dst: &mut [u8]) -> bool {
        let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_millis(10_000);
        let mut got = 0;
        while got < dst.len() {
            if self.pos < self.len {
                let take = (dst.len() - got).min(self.len - self.pos);
                dst[got..got + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
                self.pos += take;
                got += take;
                continue;
            }
            let res = embassy_futures::select::select(
                self.rx.read_packet(&mut self.buf),
                embassy_time::Timer::at(deadline),
            )
            .await;
            match res {
                embassy_futures::select::Either::First(Ok(n)) if n > 0 => {
                    self.len = n;
                    self.pos = 0;
                }
                embassy_futures::select::Either::First(_) => {
                    self.len = 0;
                    self.pos = 0;
                }
                embassy_futures::select::Either::Second(_) => return false,
            }
        }
        true
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Geiger heartbeat on the Feather D13 user LED (GP13, normal polarity).
    let led = Output::new(p.PIN_13, Level::Low);
    spawner.spawn(geiger_task(led)).unwrap();

    // Feather RP2040 ThinkInk pinout — shared between panel types.
    let cs = Output::new(p.PIN_19, Level::High);
    let dc = Output::new(p.PIN_18, Level::Low);
    let rst = Output::new(p.PIN_17, Level::High);
    let busy = Input::new(p.PIN_16, Pull::None);

    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = 4_000_000;
    let spi = Spi::new_txonly(p.SPI0, p.PIN_22, p.PIN_23, p.DMA_CH0, spi_cfg);

    let driver = UsbDriver::new(p.USB, Irqs);
    let mut config = UsbConfig::new(0x1209, 0x000d);
    config.manufacturer = Some("ferros");
    config.product = Some("eink-multi");
    config.serial_number = Some("dev-0001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CTRL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CTRL_BUF.init([0; 64]),
    );

    let state = STATE.init(State::new());
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let usb = builder.build();
    spawner.spawn(run_usb(usb)).unwrap();

    let (mut sender, mut receiver) = class.split();
    sender.wait_connection().await;

    // Panel-select handshake. Host sends 'M' + panel-id byte before any other
    // command. After this point, the chosen driver's run() takes over and
    // never returns — switching panels requires an RP2040 reset.
    say(
        &mut sender,
        b"\r\nferros eink-multi v0.1\r\n\
        Send 'M' + 1 byte to select panel:\r\n\
          0 = SSD1680 (Adafruit 6383 / 6392 grayscale)\r\n\
          1 = JD79667 (Adafruit 6414 BWRY)\r\n",
    )
    .await;

    let mode = wait_for_select(&mut receiver).await;
    match mode {
        0 => panel_ssd1680::run(spi, cs, dc, rst, busy, sender, receiver).await,
        _ => panel_jd79667::run(spi, cs, dc, rst, busy, sender, receiver).await,
    }
}

/// Block until the host sends `'M' + 1B` selecting a panel mode.
/// Any bytes received before 'M' are silently dropped (lets the firmware
/// shrug off ModemManager AT-probe noise during enumeration).
async fn wait_for_select<'d, D: UsbDriverTrait<'d>>(rx: &mut Receiver<'d, D>) -> u8 {
    let mut rs = RxStream::new(rx);
    loop {
        let b = rs.read_byte().await;
        if b == b'M' || b == b'm' {
            return rs.read_byte().await;
        }
    }
}
