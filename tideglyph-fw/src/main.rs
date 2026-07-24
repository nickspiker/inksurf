//! tideglyph BLE transfer-test firmware: connectable GATT peripheral advertising as "tideglyph" with a chunked-transfer service that BLAKE3-hashes everything written to it. This is the radio+integrity round trip for the OTA pipeline: host streams bytes → device hashes on the fly → host reads the hash back and compares. No flash writes yet — RAM/hash only.
//!
//! Transfer service protocol (128-bit UUIDs, base b5f9xxxx-2d5a-4f3c-9b1a-1d2e3f405060):
//!   ctrl  (0002, write):        [0x01] BEGIN — reset hasher + byte count. [0x02] COMMIT — finalize hash.
//!   data  (0003, write/wnr):    every write's payload is appended to the running BLAKE3.
//!   hash  (0004, read):         last COMMITted 32-byte BLAKE3.
//!   count (0005, read):         total bytes received since BEGIN, little-endian u32.
//!
//! Diagnostic LED on D9 = P1.14 (user-soldered, active high): boot stages 1=embassy, 2=MPSL, blip=rng, 3=SDC; SDC-build errors blink an errno signature (5=ENOMEM, 4=EINVAL, 6=EPERM, 3=other). After boot: LED OFF while advertising, ON while a central is connected.

#![no_std]
#![no_main]

use defmt::{info, unwrap};
use embassy_boot::{AlignedBuffer, FirmwareUpdater, FirmwareUpdaterConfig, State};
use embassy_embedded_hal::adapter::BlockingAsync;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_nrf::mode::Async;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::peripherals::{RNG, SAADC, TWISPI0};
use embassy_nrf::saadc::{self, Saadc};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::wdt::{self, Watchdog, WatchdogHandle};
use embassy_nrf::{bind_interrupts, rng};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use static_cell::StaticCell;
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // signal + att

/// BLE advertised name. Bumped per build so an OTA swap is observable on-air
/// (v1 advertises "tideglyph", the pushed v2 will announce a different name).
const ADV_NAME: &str = "tideglyph";

// Global allocator — vsf's verify path uses alloc (Vec/String while parsing the
// signed manifest). 16 KB is ample for a few-hundred-byte manifest doc.
#[global_allocator]
static HEAP: embedded_alloc::LlffHeap = embedded_alloc::LlffHeap::empty();
const HEAP_SIZE: usize = 16 * 1024;
static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

// This build's Eagle-time stamp (FIRMWARE_BUILD_STAMP) — the downgrade FLOOR
// (see build.rs). An OTA manifest is accepted only if its stamp exceeds this.
include!(concat!(env!("OUT_DIR"), "/build_stamp.rs"));

/// 256-bit shared secret for the OTA keyed-BLAKE3 MAC. Held on BOTH the device
/// (baked from ../ota_key.bin, gitignored) and the host signer (tideglyph-push).
/// Whoever holds it can author updates; whoever doesn't, can't forge one. KEEP
/// IT SECRET; recovery from a bad push is a USB double-tap reflash.
#[allow(dead_code)] // wired into COMMIT in the OTA-protocol step
const OTA_KEY: [u8; 32] = *include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ota_key.bin"));
/// Domain separation — bound into the MAC so this key can't be replayed cross-protocol.
const OTA_DOMAIN: &[u8] = b"tideglyph-ota-v1";

/// The trust-bearing fields lifted out of a manifest.
#[allow(dead_code)]
struct OtaManifest {
    stamp: i64,      // Eagle-time oscillations; must exceed FIRMWARE_BUILD_STAMP (downgrade floor)
    image_len: u32,  // raw firmware bytes staged in DFU
    mac: [u8; 32],   // keyed_hash(OTA_KEY, OTA_DOMAIN || stamp_le || image_len_le || image)
}

/// Parse + integrity-check a VSF manifest and lift the trust fields. Does NOT
/// authenticate — the caller must recompute the keyed MAC over the DFU image and
/// constant-time compare against `mac`, plus enforce the stamp floor.
#[allow(dead_code)] // wired into COMMIT in the OTA-protocol step
fn parse_manifest(manifest: &[u8]) -> Result<OtaManifest, &'static str> {
    let (header, end) = vsf::verification::read_verified(manifest, None).map_err(|_| "decode")?;
    let stamp = match &header.creation_time {
        Some(vsf::types::VsfType::e(vsf::types::EtType::e6(o))) => *o,
        _ => return Err("no stamp"),
    };
    let sections = header.sections(manifest, end).map_err(|_| "sections")?;
    let sec = sections
        .iter()
        .find(|s| s.name == "firmware.tideglyph")
        .ok_or("no fw section")?;
    let image_len = sec
        .get_fields("size")
        .first()
        .and_then(|f| f.values.first())
        .and_then(|v| match v {
            vsf::types::VsfType::z(n) => Some(*n as u32),
            _ => None,
        })
        .ok_or("no size")?;
    let mac: [u8; 32] = sec
        .get_fields("mac")
        .first()
        .and_then(|f| f.values.first())
        .and_then(|v| match v {
            vsf::types::VsfType::gH(h) if h.len() == 32 => h.as_slice().try_into().ok(),
            _ => None,
        })
        .ok_or("no mac")?;
    Ok(OtaManifest { stamp, image_len, mac })
}

/// Constant-time 32-byte compare (don't leak MAC bytes via timing).
#[allow(dead_code)]
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// D9 = P1.14 diagnostic LED, direct registers (independent of any HAL state).
const P1_BASE: u32 = 0x5000_0300;
const DIRSET: u32 = P1_BASE + 0x518;
const OUTSET: u32 = P1_BASE + 0x508;
const OUTCLR: u32 = P1_BASE + 0x50C;
const LED: u32 = 14;

fn led_init() {
    unsafe { core::ptr::write_volatile(DIRSET as *mut u32, 1 << LED) }
}
fn led(on: bool) {
    let reg = if on { OUTSET } else { OUTCLR };
    unsafe { core::ptr::write_volatile(reg as *mut u32, 1 << LED) }
}
async fn stage(n: u32) {
    for _ in 0..n {
        led(true);
        Timer::after(Duration::from_millis(120)).await;
        led(false);
        Timer::after(Duration::from_millis(120)).await;
    }
    Timer::after(Duration::from_millis(700)).await;
}

// Runs before statics init. The app is the ACTIVE image at 0x08000, launched by the embassy-boot second stage. embassy-boot's load() sets VTOR here on handoff, but we re-assert it in pre_init so exceptions vector to our table through the app's own reset path too. (FLASH ORIGIN = ACTIVE = 0x08000.)
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    (0xE000_ED08 as *mut u32).write_volatile(0x0000_8000);
}

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<RNG>;
    SAADC => saadc::InterruptHandler;
    TWISPI0 => spim::InterruptHandler<TWISPI0>;
    EGU0_SWI0 => nrf_sdc::mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_sdc::mpsl::ClockInterruptHandler;
    RADIO => nrf_sdc::mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
});

// Battery-voltage calibration for the XIAO ONBOARD divider (~÷3) on AIN7/P0.31,
// gated by VBAT_ENABLE (P0.14). Calibrated against a known 4.20V cell: raw = 1599,
// so batt_mV = raw * 4200/1599. Linear through origin — the onboard tap is a clean
// resistive divider on BAT+ (no board pull), so one point is enough.
const BATT_CAL_NUM: u32 = 4200;
const BATT_CAL_DEN: u32 = 1599;

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

/// Pet the bootloader's watchdog. The embassy-boot second stage starts the WDT
/// (WatchdogFlash) and the nRF WDT cannot be stopped once running, so EVERY boot
/// (not just trial boots) must pet it or the chip resets in ~5 s. A hung app
/// therefore stops petting → WDT fires → bootloader reverts to the previous image.
#[embassy_executor::task]
async fn wdt_pet(mut handle: WatchdogHandle) -> ! {
    loop {
        handle.pet();
        Timer::after(Duration::from_secs(2)).await;
    }
}

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<Async>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .build(p, rng, mpsl, mem)
}

/// Battery read via the XIAO's ONBOARD ~÷3 divider on AIN7/P0.31, gated on by
/// VBAT_ENABLE (P0.14, active low). 0.6V ref + 1/6 gain (3.6V FS) + 40µs
/// acquisition, 16x averaged. Returns (millivolts, raw 12-bit count). Confirmed
/// on HW: raw 1599 at a 4.20V cell. NOTE for panel integration: P0.31 also carries
/// the panel DC net, so during a read set P0.31 to analog + enable the divider,
/// and restore it to the DC output between reads (they never overlap).
async fn read_battery(
    saadc_dev: embassy_nrf::Peri<'static, SAADC>,
    ain7: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_31>,
    vbat_en: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_14>,
) -> (u16, u16) {
    // Enable the onboard divider (active low), let it settle before sampling.
    let _en = Output::new(vbat_en, Level::Low, OutputDrive::Standard);
    Timer::after(Duration::from_millis(5)).await;
    let mut cc = saadc::ChannelConfig::single_ended(ain7);
    cc.gain = saadc::Gain::Gain1_6;
    cc.reference = saadc::Reference::Internal;
    cc.time = saadc::Time::_40US;
    let mut adc = Saadc::new(saadc_dev, Irqs, saadc::Config::default(), [cc]);
    let mut buf = [0i16; 1];
    adc.sample(&mut buf).await; // discard first (settling)
    let mut acc = 0i32;
    for _ in 0..16 {
        adc.sample(&mut buf).await;
        acc += buf[0] as i32;
    }
    let raw = (acc / 16).clamp(0, 4095) as u16;
    let mv = (raw as u32 * BATT_CAL_NUM / BATT_CAL_DEN) as u16;
    (mv, raw)
}

// GATT server: OTA service.
//   ctrl   BEGIN [0x01, image_len u32 LE, manifest_len u16 LE]; COMMIT [0x02]
//   data   image bytes (image_len) then manifest bytes (manifest_len)
//   status [state u8, verdict u8, received u32 LE]
//          verdict: 0 idle, 1 receiving, 3 ACCEPTED (swap+reset imminent),
//                   10 size-mismatch, 11 stale-stamp, 12 bad-MAC, 13 decode-fail
#[gatt_server]
struct Server {
    ota: OtaService,
}

#[gatt_service(uuid = "b5f90001-2d5a-4f3c-9b1a-1d2e3f405060")]
struct OtaService {
    #[characteristic(uuid = "b5f90002-2d5a-4f3c-9b1a-1d2e3f405060", write, value = [0u8; 8])]
    ctrl: [u8; 8],
    #[characteristic(uuid = "b5f90003-2d5a-4f3c-9b1a-1d2e3f405060", write, write_without_response, value = [0u8; 244])]
    data: [u8; 244],
    #[characteristic(uuid = "b5f90004-2d5a-4f3c-9b1a-1d2e3f405060", read, value = [0u8; 6])]
    status: [u8; 6],
    /// Battery: [mV u16 LE, raw u16 LE] — last SAADC read.
    #[characteristic(uuid = "b5f90005-2d5a-4f3c-9b1a-1d2e3f405060", read, value = [0u8; 4])]
    battery: [u8; 4],
}

const V_RECEIVING: u8 = 1;
const V_ACCEPTED: u8 = 3;
const V_SIZE: u8 = 10;
const V_STALE: u8 = 11;
const V_BADMAC: u8 = 12;
const V_DECODE: u8 = 13;

// JD79667 panel (Adafruit 6414 BWRY) on the EN04/EN05 board. Chip-internal
// geometry 180 wide × 384 tall, 2 bits/px (4 px/byte, MSB first). Init/timing
// ported from inksurf's proven src/panel_jd79667.rs. BUSY LOW = busy.
const PW: usize = 180;
const PH: usize = 384;
const PROW: usize = PW / 4; // 45 bytes/row
const FB_BYTES: usize = 17_664;
const C_BLACK: u8 = 0b00;
const C_WHITE: u8 = 0b01;
const C_YELLOW: u8 = 0b10;
const C_RED: u8 = 0b11;
static mut PANEL_FB: [u8; FB_BYTES] = [0u8; FB_BYTES];

struct Panel<'d> {
    spi: Spim<'d>,
    cs: Output<'d>,
    dc: Output<'d>,
    rst: Output<'d>,
    busy: Input<'d>,
    en: Output<'d>,
}

impl<'d> Panel<'d> {
    /// Wait for BUSY to go HIGH (idle). Async so the WDT-pet task runs meanwhile.
    async fn wait_ready(&mut self) {
        for _ in 0..30_000 {
            if self.busy.is_high() {
                return;
            }
            Timer::after(Duration::from_millis(1)).await;
        }
    }
    async fn cmd(&mut self, c: u8, data: &[u8]) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[c]).await;
        self.dc.set_high();
        if !data.is_empty() {
            let _ = self.spi.write(data).await;
        }
        self.cs.set_high();
    }
    async fn init(&mut self) {
        self.en.set_high(); // power the panel
        Timer::after(Duration::from_millis(10)).await;
        self.rst.set_low();
        Timer::after(Duration::from_millis(50)).await;
        self.rst.set_high();
        Timer::after(Duration::from_millis(100)).await;
        info!("[panel] post-reset BUSY high? {}", self.busy.is_high());
        self.wait_ready().await;
        info!("[panel] ready, sending init");
        self.cmd(0x4D, &[0x78]).await;
        self.cmd(0x00, &[0x0F, 0x29]).await; // PSR
        self.cmd(0x01, &[0x07, 0x00]).await; // PWRR
        self.cmd(0x03, &[0x10, 0x54, 0x44]).await; // POFS
        self.cmd(0x06, &[0x05, 0x00, 0x3F, 0x0A, 0x25, 0x12, 0x1A]).await; // BTST
        self.cmd(0x50, &[0x37]).await; // CDI
        self.cmd(0x60, &[0x02, 0x02]).await; // TCON
        self.cmd(0x61, &[0x00, 0xB4, 0x01, 0x80]).await; // TRES 180×384
        self.cmd(0xE7, &[0x1C]).await;
        self.cmd(0xE3, &[0x22]).await;
        self.cmd(0xB4, &[0xD0]).await;
        self.cmd(0xB5, &[0x03]).await;
        self.cmd(0xE9, &[0x01]).await;
        self.cmd(0x30, &[0x08]).await; // PLL
        self.cmd(0x04, &[]).await; // POWER ON
        self.wait_ready().await;
    }
    /// Stream the framebuffer (DTM 0x10) and refresh (DRF 0x12). ~15s async.
    async fn push(&mut self, fb: &[u8]) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.write(&[0x10]).await; // DTM
        self.dc.set_high();
        let _ = self.spi.write(fb).await;
        self.cs.set_high();
        info!("[panel] framebuffer sent, refreshing; BUSY high? {}", self.busy.is_high());
        self.cmd(0x12, &[0x00]).await; // DRF refresh
        Timer::after(Duration::from_millis(50)).await;
        info!("[panel] DRF sent, BUSY high (should be LOW=busy now)? {}", self.busy.is_high());
        self.wait_ready().await;
    }
}

// Tide render. User-facing canvas is 384 wide × 180 tall (landscape), one colour
// code per pixel; pack() mirrors it into the 180×384 chip framebuffer.
const CW: usize = 384;
const CH: usize = 180;
const TIDE_MIN_FT: f32 = -4.61; // MLLW y-axis bounds (match tide-display)
const TIDE_MAX_FT: f32 = 14.09;
const MSL_TO_MLLW: f32 = 6.814; // tide-core predicts MSL; display axis is MLLW
const N_SAMPLES: usize = 241; // ±12h at 6-min steps
// Zero-init so it lands in .bss (RAM only, not stored in FLASH). render_tide
// fills it white before drawing, so the initial value doesn't matter.
static mut CANVAS: [u8; CW * CH] = [0u8; CW * CH];

fn interp(s: &[f32; N_SAMPLES], si: f32) -> f32 {
    if si <= 0.0 {
        return s[0];
    }
    if si >= (N_SAMPLES - 1) as f32 {
        return s[N_SAMPLES - 1];
    }
    let i = si as usize;
    let frac = si - i as f32;
    s[i] + frac * (s[i + 1] - s[i])
}

/// Render a Bremerton tide chart centred on `unix` seconds into CANVAS, then
/// pack into PANEL_FB. MVP: white bg, yellow tide-fill curve, black hourly ticks
/// + a black "now" line down the centre. (Sun/moon/labels come later.)
async fn render_tide(unix: i64) {
    let canvas = unsafe { &mut *core::ptr::addr_of_mut!(CANVAS) };
    for p in canvas.iter_mut() {
        *p = C_WHITE;
    }
    // 241 tide samples over ±12h, MLLW feet. Software f64 trig is slow, so yield
    // periodically — otherwise this synchronous loop starves the WDT-pet task and
    // the watchdog reboots us mid-render.
    let mut samples = [0f32; N_SAMPLES];
    let base = unix - 12 * 3600;
    for i in 0..N_SAMPLES {
        let t = (base + 6 * 60 * i as i64) as f64;
        samples[i] = tide_core::predict(tide_core::BREMERTON, t) as f32 + MSL_TO_MLLW;
        if i % 8 == 0 {
            embassy_futures::yield_now().await;
        }
    }
    // Curve fill: 24h window, now at centre; yellow from the curve to the bottom.
    let range = TIDE_MAX_FT - TIDE_MIN_FT;
    for x in 0..CW {
        let secs_from_now = (x as f32 - CW as f32 / 2.0) / CW as f32 * 86400.0;
        let si = (secs_from_now + 12.0 * 3600.0) / 360.0; // sample index
        let h = interp(&samples, si);
        let tt = ((h - TIDE_MIN_FT) / range).clamp(0.0, 1.0);
        let y = ((CH as f32 - 1.0) - tt * (CH as f32 - 1.0)) as usize;
        for yy in y..CH {
            canvas[yy * CW + x] = C_YELLOW;
        }
    }
    // Hourly ticks (black, top + bottom edge).
    for hh in -12..=12i32 {
        let x = (CW as f32 / 2.0 + hh as f32 * 3600.0 / 86400.0 * CW as f32) as i32;
        if x >= 0 && x < CW as i32 {
            canvas[x as usize] = C_BLACK;
            canvas[(CH - 1) * CW + x as usize] = C_BLACK;
        }
    }
    // "Now" line, black, down the centre.
    let nx = CW / 2;
    for y in 0..CH {
        canvas[y * CW + nx] = C_BLACK;
    }
    pack(canvas);
}

/// Mirror the landscape canvas into the chip framebuffer (2bpp, x-mirrored, MSB
/// first) — the pack_to_chip transform from tide-display.
fn pack(canvas: &[u8; CW * CH]) {
    let fb = unsafe { &mut *core::ptr::addr_of_mut!(PANEL_FB) };
    for b in fb.iter_mut() {
        *b = 0x55; // all white
    }
    for y in 0..CH {
        for x in 0..CW {
            let code = canvas[y * CW + x] & 0x3;
            let chip_row = CW - 1 - x;
            let chip_col = y;
            let byte_idx = chip_row * PROW + chip_col / 4;
            let shift = (3 - (chip_col % 4)) * 2;
            let mask = !(0b11u8 << shift);
            fb[byte_idx] = (fb[byte_idx] & mask) | (code << shift);
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }

    let p = embassy_nrf::init(Default::default());
    led_init();

    // Inherit + pet the bootloader's watchdog (N=1 handle, matching WatchdogFlash).
    // Config::try_new returns Some only when the WDT is already running — i.e. we
    // booted via the embassy-boot stage. Booted bare (no bootloader), it's None
    // and there's nothing to pet.
    if let Some(cfg) = wdt::Config::try_new(&p.WDT) {
        if let Ok((_wdt, [h])) = Watchdog::try_new::<_, 1>(p.WDT, cfg) {
            spawner.spawn(unwrap!(wdt_pet(h)));
        }
    }

    stage(1).await; // embassy init OK

    let mpsl_p = mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_CTIV as u8,
        rc_temp_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_TEMP_CTIV as u8,
        accuracy_ppm: mpsl::raw::MPSL_DEFAULT_CLOCK_ACCURACY_PPM as u16,
        skip_wait_lfclk_started: mpsl::raw::MPSL_DEFAULT_SKIP_WAIT_LFCLK_STARTED != 0,
    };
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(unwrap!(mpsl::MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk_cfg)));
    spawner.spawn(unwrap!(mpsl_task(&*mpsl)));
    stage(2).await; // MPSL up

    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24, p.PPI_CH25, p.PPI_CH26,
        p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    let mut rng = rng::Rng::new(p.RNG, Irqs);
    // Quick pre-marker: rng + peripherals constructed.
    led(true);
    Timer::after(Duration::from_millis(60)).await;
    led(false);
    Timer::after(Duration::from_millis(300)).await;

    // 8192: GATT peripheral needs more than the advertise-only 4096 (the example's GATT config uses 4720; ENOMEM here blinks 5s).
    let mut sdc_mem = sdc::Mem::<8192>::new();
    let sdc = match build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem) {
        Ok(s) => s,
        Err(e) => {
            let n = if e == nrf_sdc::Error::ENOMEM {
                5
            } else if e == nrf_sdc::Error::EINVAL {
                4
            } else if e == nrf_sdc::Error::EPERM {
                6
            } else {
                3
            };
            loop {
                for _ in 0..n {
                    led(true);
                    Timer::after(Duration::from_millis(400)).await;
                    led(false);
                    Timer::after(Duration::from_millis(250)).await;
                }
                Timer::after(Duration::from_millis(1500)).await;
            }
        }
    };
    stage(3).await; // SDC built

    // Battery read is skipped this build: P0.31 is time-shared with panel DC and
    // the panel owns it here. (Battery time-share comes with the render loop.)
    let batt = (0u16, 0u16);
    let _ = read_battery; // keep it compiled for the next milestone

    // OTA flash: the app's own Nvmc behind a Mutex, shared with per-connection
    // FirmwareUpdaters. from_linkerfile reads the __bootloader_dfu/state symbols.
    let nvmc = Mutex::<NoopRawMutex, _>::new(BlockingAsync::new(Nvmc::new(p.NVMC)));

    // Drive the panel: render the tide chart + refresh (embassy-nrf SPIM port of
    // the proven JD79667 driver). The render yields and the ~15s refresh is async,
    // so the WDT-pet task keeps running. mark_booted is DELIBERATELY after this so
    // a bad render/panel that hangs never confirms -> WDT -> bootloader reverts.
    {
        let mut sc = spim::Config::default();
        sc.frequency = spim::Frequency::M8;
        let spi = Spim::new_txonly(p.TWISPI0, Irqs, p.P1_13, p.P1_15, sc);
        let mut panel = Panel {
            spi,
            cs: Output::new(p.P1_12, Level::High, OutputDrive::Standard),
            dc: Output::new(p.P0_31, Level::Low, OutputDrive::Standard),
            rst: Output::new(p.P0_15, Level::High, OutputDrive::Standard),
            busy: Input::new(p.P0_29, Pull::None),
            en: Output::new(p.P1_11, Level::Low, OutputDrive::Standard),
        };
        render_tide(BUILD_UNIX_SECS).await; // fills PANEL_FB with the tide chart
        panel.init().await;
        let fb = unsafe { &*core::ptr::addr_of!(PANEL_FB) };
        panel.push(fb).await;
        info!("[panel] tide chart refreshed for unix {}", BUILD_UNIX_SECS);
    }

    // Confirm the boot ONLY after a successful render + panel drive (the self-test).
    // A bad render/panel that hangs never reaches here -> WDT -> bootloader reverts
    // to the previous good image, no cable needed.
    {
        let mut aligned = AlignedBuffer([0u8; 4]);
        let cfg = FirmwareUpdaterConfig::from_linkerfile(&nvmc, &nvmc);
        let mut updater = FirmwareUpdater::new(cfg, &mut aligned.0);
        if let Ok(State::Revert) = updater.get_state().await {
            info!("[ota] previous update was REVERTED by the bootloader");
        }
        let _ = updater.mark_booted().await;
    }

    run(sdc, &nvmc, batt).await;
}

async fn run<C, F>(controller: C, nvmc: &Mutex<NoopRawMutex, F>, batt: (u16, u16))
where
    C: Controller,
    F: embedded_storage_async::nor_flash::NorFlash,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let server = unwrap!(Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: ADV_NAME,
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    })));

    // Publish the boot-time battery reading: [mV u16 LE, raw u16 LE].
    let mut bch = [0u8; 4];
    bch[0..2].copy_from_slice(&batt.0.to_le_bytes());
    bch[2..4].copy_from_slice(&batt.1.to_le_bytes());
    let _ = server.set(&server.ota.battery, &bch);

    let _ = join(ble_task(runner), async {
        loop {
            led(false); // advertising: LED off
            match advertise(ADV_NAME, &mut peripheral, &server).await {
                Ok(conn) => {
                    led(true); // connected: LED on
                    // Fresh updater per connection: write_firmware's last-erased
                    // tracking resets, so a new push always erases sector 0 before
                    // writing (stale tracking across pushes would corrupt DFU).
                    let mut aligned = AlignedBuffer([0u8; 4]);
                    let cfg = FirmwareUpdaterConfig::from_linkerfile(nvmc, nvmc);
                    let mut updater = FirmwareUpdater::new(cfg, &mut aligned.0);
                    ota_session(&server, &conn, &mut updater).await;
                }
                Err(_) => {
                    for _ in 0..3 {
                        led(true);
                        Timer::after(Duration::from_millis(60)).await;
                        led(false);
                        Timer::after(Duration::from_millis(60)).await;
                    }
                }
            }
        }
    })
    .await;
}

async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            let e = defmt::Debug2Format(&e);
            panic!("[ble_task] error: {:?}", e);
        }
    }
}

/// Verify a staged update: parse the manifest, enforce the stamp floor, then
/// recompute the keyed-BLAKE3 MAC over the DFU-flashed image (what will actually
/// boot) and constant-time compare. Returns a verdict code.
async fn ota_verify<DFU, STATE>(
    updater: &mut FirmwareUpdater<'_, DFU, STATE>,
    manifest: &[u8],
    image_len: u32,
) -> u8
where
    DFU: embedded_storage_async::nor_flash::NorFlash,
    STATE: embedded_storage_async::nor_flash::NorFlash,
{
    let m = match parse_manifest(manifest) {
        Ok(m) => m,
        Err(_) => return V_DECODE,
    };
    if m.image_len != image_len {
        return V_SIZE;
    }
    if m.stamp <= FIRMWARE_BUILD_STAMP {
        return V_STALE; // downgrade floor — never accept an image older than the running build
    }
    let mut h = blake3::Hasher::new_keyed(&OTA_KEY);
    h.update(OTA_DOMAIN);
    h.update(&m.stamp.to_le_bytes());
    h.update(&image_len.to_le_bytes());
    let mut off = 0u32;
    let mut buf = [0u8; 256];
    while off < image_len {
        let n = core::cmp::min(256, (image_len - off) as usize);
        if updater.read_dfu(off, &mut buf[..n]).await.is_err() {
            return V_DECODE;
        }
        h.update(&buf[..n]);
        off += n as u32;
    }
    if ct_eq(h.finalize().as_bytes(), &m.mac) {
        V_ACCEPTED
    } else {
        V_BADMAC
    }
}

/// Handle the OTA service for one connection: BEGIN sets sizes, data streams the
/// image into DFU + buffers the manifest, COMMIT verifies + (if good) swaps.
async fn ota_session<P, DFU, STATE>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    updater: &mut FirmwareUpdater<'_, DFU, STATE>,
) where
    P: PacketPool,
    DFU: embedded_storage_async::nor_flash::NorFlash,
    STATE: embedded_storage_async::nor_flash::NorFlash,
{
    let ctrl_h = server.ota.ctrl;
    let data_h = server.ota.data;
    let status_ch = server.ota.status;

    let mut image_len = 0u32;
    let mut manifest_len = 0usize;
    let mut img_recv = 0u32;
    let mut man_recv = 0usize;
    let mut manifest_buf = [0u8; 512];

    let set_status = |verdict: u8, recv: u32| {
        let mut st = [0u8; 6];
        st[0] = 1;
        st[1] = verdict;
        st[2..6].copy_from_slice(&recv.to_le_bytes());
        let _ = server.set(&status_ch, &st);
    };

    let mut commit_pending = false;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { .. } => break,
            GattConnectionEvent::Gatt { event } => {
                let reply = match event {
                    GattEvent::Read(event) => event.accept(),
                    GattEvent::Write(event) => {
                        if event.handle() == data_h.handle {
                            let mut chunk = [0u8; 244];
                            let mut clen = 0usize;
                            event.with_data(|_o, b| {
                                clen = b.len().min(244);
                                chunk[..clen].copy_from_slice(&b[..clen]);
                            });
                            // Route: image bytes -> DFU (aligned writes), then manifest -> RAM.
                            let mut consumed = 0usize;
                            if img_recv < image_len {
                                let n = core::cmp::min((image_len - img_recv) as usize, clen);
                                if updater.write_firmware(img_recv as usize, &chunk[..n]).await.is_ok() {
                                    img_recv += n as u32;
                                }
                                consumed = n;
                            }
                            if consumed < clen && man_recv < manifest_len {
                                let n = core::cmp::min(manifest_len - man_recv, clen - consumed)
                                    .min(manifest_buf.len() - man_recv);
                                manifest_buf[man_recv..man_recv + n]
                                    .copy_from_slice(&chunk[consumed..consumed + n]);
                                man_recv += n;
                            }
                            set_status(V_RECEIVING, img_recv);
                            event.accept_unprocessed()
                        } else if event.handle() == ctrl_h.handle {
                            let mut cb = [0u8; 8];
                            let mut cn = 0usize;
                            event.with_data(|_o, b| {
                                cn = b.len().min(8);
                                cb[..cn].copy_from_slice(&b[..cn]);
                            });
                            if cn >= 7 && cb[0] == 0x01 {
                                image_len = u32::from_le_bytes(cb[1..5].try_into().unwrap());
                                manifest_len = u16::from_le_bytes(cb[5..7].try_into().unwrap()) as usize;
                                img_recv = 0;
                                man_recv = 0;
                                info!("[ota] BEGIN image={} manifest={}", image_len, manifest_len);
                                set_status(V_RECEIVING, 0);
                            } else if cn >= 1 && cb[0] == 0x02 {
                                // Reply to COMMIT FAST (status=verifying); do the heavy
                                // verify AFTER the reply so the host's write doesn't time out.
                                set_status(2, img_recv);
                                commit_pending = true;
                            }
                            event.accept_unprocessed()
                        } else {
                            event.accept()
                        }
                    }
                    _ => event.accept(),
                };
                if let Ok(r) = reply {
                    r.send().await;
                }
                if commit_pending {
                    commit_pending = false;
                    // Fast-reject an incomplete transfer without reading DFU.
                    let verdict = if img_recv != image_len || man_recv != manifest_len {
                        V_SIZE
                    } else {
                        ota_verify(updater, &manifest_buf[..man_recv], image_len).await
                    };
                    info!("[ota] COMMIT verdict={} img_recv={}", verdict, img_recv);
                    set_status(verdict, img_recv);
                    if verdict == V_ACCEPTED {
                        let _ = updater.mark_updated().await;
                        Timer::after(Duration::from_secs(1)).await; // let host read status
                        cortex_m::peripheral::SCB::sys_reset();
                    }
                }
            }
            _ => {}
        }
    }
}

async fn advertise<'values, 'server, C: Controller>(
    name: &'values str,
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    Ok(conn)
}
