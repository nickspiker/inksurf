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
use embassy_futures::join::join3;
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
const DIRCLR: u32 = P1_BASE + 0x51C;
const LED: u32 = 14;

// Park the XIAO onboard RGB user LED (P0.26 R, P0.30 G, P0.06 B — active LOW) fully
// OFF: drive each HIGH via direct registers so it persists without a HAL pin object
// to drop (a dropped Output reverts to input, and we never touch these in the HAL).
// HIGH not LOW — LOW is the ON state for an active-low LED. OUTSET before DIRSET so
// the pin latches high the instant it becomes an output (never a low glitch = glow).
// Insurance against any onboard LED being the dim-green the user saw; the battery
// charge LED is on the BQ25101, not a GPIO, so this can't affect that one.
fn park_rgb_leds() {
    const P0_BASE: u32 = 0x5000_0000;
    for pin in [26u32, 30, 6] {
        unsafe {
            core::ptr::write_volatile((P0_BASE + 0x508) as *mut u32, 1 << pin); // OUTSET = high
            core::ptr::write_volatile((P0_BASE + 0x700 + pin * 4) as *mut u32, 1 | (1 << 1)); // DIR=out, input disconnect
        }
    }
}

// D9/P1.14 user LED (user-soldered, active-high). Two states only, and "hot" (full
// current) is electrically UNREACHABLE because we never drive the pin high:
//   off  = OUTPUT LOW  → 0V across the LED, zero current, no glow.
//   busy = INPUT/FLOAT → high-Z; the LED lights only from ~µA pin leakage, a
//          ~100x-dimmer "doing something" glow. Input buffer stays disconnected
//          (reset default) so float is pure leakage, not CMOS shoot-through.
// The OUT latch is left at 0 forever — we only ever toggle DIR — so a bright/hot
// drive cannot happen even by accident. led_init parks it OFF.
fn led_init() {
    unsafe {
        core::ptr::write_volatile(OUTCLR as *mut u32, 1 << LED); // OUT latch low (kept low forever)
        core::ptr::write_volatile(DIRSET as *mut u32, 1 << LED); // output-low = off
    }
}
fn led(busy: bool) {
    unsafe {
        if busy {
            core::ptr::write_volatile(DIRCLR as *mut u32, 1 << LED); // float = dim leakage glow
        } else {
            core::ptr::write_volatile(DIRSET as *mut u32, 1 << LED); // output-low = off (OUT still 0)
        }
    }
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
    /// Battery + liveness: [mV u16 LE, raw u16 LE, refresh_count u16 LE]. The
    /// count increments every refresh, so a reader can confirm the clock is ticking.
    #[characteristic(uuid = "b5f90005-2d5a-4f3c-9b1a-1d2e3f405060", read, value = [0u8; 6])]
    battery: [u8; 6],
    #[characteristic(uuid = "b5f90006-2d5a-4f3c-9b1a-1d2e3f405060", write, value = [0u8; 8])]
    time: [u8; 8],
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
        self.cmd(0x12, &[0x00]).await; // DRF refresh
        self.wait_ready().await;
        // POWER OFF (0x02): shut the drivers down so the image is passively
        // retained. Without this the panel stays actively biased and the image
        // slowly fades/ghosts. (Deep-sleep 0x07 deferred — needs a verified wake
        // sequence before we risk it on the glued unit.)
        self.cmd(0x02, &[]).await;
        Timer::after(Duration::from_millis(100)).await;
        info!("[panel] refreshed + powered off");
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

// Bremerton (Southworth WA) for the solar day/night calc.
const SUN_LAT: f64 = 47.5126;
const SUN_LON: f64 = -122.5054;

/// Sun altitude (degrees) at `unix`, seen from SUN_LAT/LON. Meeus low-precision
/// solar position straight from the Unix instant (no dates/timezones), same
/// ecliptic→horizontal pipeline as the moon calc. `> 0` = sun up. Host-verified:
/// +58° summer noon, −20° midnight, +21° winter noon.
fn sun_altitude_deg(unix: i64) -> f64 {
    const J2000_UNIX: f64 = 946_728_000.0;
    let d = (unix as f64 - J2000_UNIX) / 86_400.0;
    let rad = |x: f64| x * core::f64::consts::PI / 180.0;
    let norm = |x: f64| {
        let m = libm::fmod(x, 360.0);
        if m < 0.0 {
            m + 360.0
        } else {
            m
        }
    };
    let l = norm(280.460 + 0.9856474 * d);
    let g = norm(357.528 + 0.9856003 * d);
    let lam = rad(l + 1.915 * libm::sin(rad(g)) + 0.020 * libm::sin(rad(2.0 * g)));
    let eps = rad(23.439 - 0.0000004 * d);
    let ra = libm::atan2(libm::cos(eps) * libm::sin(lam), libm::cos(lam));
    let dec = libm::asin(libm::sin(eps) * libm::sin(lam));
    let gmst = norm(280.46061837 + 360.98564736629 * d);
    let lst = rad(norm(gmst + SUN_LON));
    let ha = lst - ra;
    let lat = rad(SUN_LAT);
    libm::asin(libm::sin(lat) * libm::sin(dec) + libm::cos(lat) * libm::cos(dec) * libm::cos(ha))
        * 180.0
        / core::f64::consts::PI
}

/// Moon altitude (degrees) at `unix` from SUN_LAT/LON — Meeus low-precision lunar
/// theory straight from the Unix instant. `> 0` = moon up. Same ecliptic→horizontal
/// pipeline as the sun, plus the dominant lunar periodic terms. Cross-checked
/// against tide-display's reference to < 1e-13°.
fn moon_altitude_deg(unix: i64) -> f64 {
    const J2000_UNIX: f64 = 946_728_000.0;
    let d = (unix as f64 - J2000_UNIX) / 86_400.0;
    let rad = |x: f64| x * core::f64::consts::PI / 180.0;
    let norm = |x: f64| {
        let m = libm::fmod(x, 360.0);
        if m < 0.0 {
            m + 360.0
        } else {
            m
        }
    };
    let lp = norm(218.316 + 13.176396 * d); // mean longitude
    let m = norm(134.963 + 13.064993 * d); // mean anomaly
    let f = norm(93.272 + 13.229350 * d); // argument of latitude
    let dd = norm(297.850 + 12.190749 * d); // mean elongation
    let lambda = lp + 6.289 * libm::sin(rad(m)) - 1.274 * libm::sin(rad(2.0 * (lp - dd) - m))
        + 0.658 * libm::sin(rad(2.0 * (lp - dd)))
        - 0.186 * libm::sin(rad(norm(357.529 + 0.985600 * d)));
    let beta =
        5.128 * libm::sin(rad(f)) + 0.281 * libm::sin(rad(m + f)) - 0.278 * libm::sin(rad(f - m));
    let eps = rad(23.439 - 0.0000004 * d);
    let lam = rad(lambda);
    let bet = rad(beta);
    let ra = libm::atan2(
        libm::sin(lam) * libm::cos(eps) - libm::tan(bet) * libm::sin(eps),
        libm::cos(lam),
    );
    let dec = libm::asin(
        libm::sin(bet) * libm::cos(eps) + libm::cos(bet) * libm::sin(eps) * libm::sin(lam),
    );
    let gmst = norm(280.46061837 + 360.98564736629 * d);
    let lst = rad(norm(gmst + SUN_LON));
    let ha = lst - ra;
    let lat = rad(SUN_LAT);
    libm::asin(libm::sin(lat) * libm::sin(dec) + libm::cos(lat) * libm::cos(dec) * libm::cos(ha))
        * 180.0
        / core::f64::consts::PI
}

/// Day→night colour swap (BLACK↔WHITE, YELLOW↔RED).
fn invert_code(c: u8) -> u8 {
    match c {
        C_BLACK => C_WHITE,
        C_WHITE => C_BLACK,
        C_YELLOW => C_RED,
        C_RED => C_YELLOW,
        _ => c,
    }
}

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

// Decimal bitmap font (0-9 then colon at index 10), decoded from PNGs at build time.
mod font {
    include!(concat!(env!("OUT_DIR"), "/font_glyphs.rs"));
}

// US Pacific local time, DST-aware. Pure integer calendar math (Howard Hinnant),
// no_std — day-number ↔ civil date, then the US DST window.
fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
fn days_from_civil(y0: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y0 - 1 } else { y0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + (d as i64 - 1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
/// Unix seconds of `hour_utc` on the `n`th Sunday of `month` in `year`.
fn nth_sunday_unix(year: i64, month: u32, n: i64, hour_utc: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let wd = (first + 4).rem_euclid(7); // 0 = Sunday (1970-01-01 was Thursday)
    let first_sunday_dom = 1 + ((7 - wd) % 7);
    days_from_civil(year, month, (first_sunday_dom + 7 * (n - 1)) as u32) * 86400 + hour_utc * 3600
}
/// Pacific offset at `unix`: PDT (-7h) inside the US DST window (2nd Sun Mar 02:00
/// → 1st Sun Nov 02:00 local), else PST (-8h). Host-verified vs the real 2025-27
/// transition dates.
fn tz_offset_secs(unix: i64) -> i64 {
    let (year, _, _) = civil_from_days(unix.div_euclid(86400));
    let dst_start = nth_sunday_unix(year, 3, 2, 10); // 02:00 PST = 10:00 UTC
    let dst_end = nth_sunday_unix(year, 11, 1, 9); // 02:00 PDT = 09:00 UTC
    if unix >= dst_start && unix < dst_end {
        -7 * 3600
    } else {
        -8 * 3600
    }
}

fn local_hh_mm(unix: i64) -> (u32, u32) {
    let sod = (unix + tz_offset_secs(unix)).rem_euclid(86400);
    ((sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

fn blit_glyph(canvas: &mut [u8], g: &font::Glyph, left: i32, top: i32, color: u8) {
    let w = g.w as i32;
    for gy in 0..g.h as i32 {
        let py = top + gy;
        if py < 0 || py >= CH as i32 {
            continue;
        }
        for gx in 0..w {
            let px = left + gx;
            if px < 0 || px >= CW as i32 {
                continue;
            }
            if g.bits[(gy * w + gx) as usize] != 0 {
                canvas[py as usize * CW + px as usize] = color;
            }
        }
    }
}

/// Wall-clock → (hi, lo) dozenal-symbol indices, rounded to the nearest 10-min
/// mark: a 2-digit base-12 odometer of the day's 144 ten-minute marks. Wraps at
/// midnight. Mirrors tide-display's dozenal_indices exactly.
fn dozenal_indices(hh: u32, mm: u32) -> (usize, usize) {
    let counter = ((hh * 60 + mm + 5) / 10) % 144;
    ((counter / 12) as usize, (counter % 12) as usize)
}

/// Stamp the 2-symbol dozenal time around the now-line: hi's right edge at
/// `anchor_x`, lo's left edge at `anchor_x + 1`, so the line shows through the gap.
/// A trailing Zil (lo == 0) is dropped and the lone hi centres on the line — so
/// midnight (Zil Zil) renders a single Zil sitting on it. Mirrors tide-display.
fn draw_dozenal_label(canvas: &mut [u8], hi: usize, lo: usize, anchor_x: i32, top_y: i32, color: u8) {
    let d = &font::DOZENAL;
    if lo == 0 {
        let g = &d[hi];
        blit_glyph(canvas, g, anchor_x - g.w as i32 / 2, top_y, color);
    } else {
        blit_glyph(canvas, &d[hi], anchor_x - d[hi].w as i32, top_y, color);
        blit_glyph(canvas, &d[lo], anchor_x + 1, top_y, color);
    }
}

/// Stamp the dozenal time rotated 90° for a sunrise/sunset column: CCW reading
/// bottom→top for sunrise, CW reading top→bottom for sunset, stacked and centred
/// on (center_x, center_y). Each "on" pixel inverts what's beneath (reads against
/// day or night). Trailing Zil dropped. Mirrors tide-display exactly.
fn draw_dozenal_rotated_invert(
    canvas: &mut [u8],
    hi: usize,
    lo: usize,
    center_x: f32,
    center_y: i32,
    sunrise: bool,
) {
    let d = &font::DOZENAL;
    let count: i32 = if lo == 0 { 1 } else { 2 };
    let total_h: i32 = d[hi].w as i32
        + if lo == 0 { 0 } else { d[lo].w as i32 }
        + font::GLYPH_KERN * (count - 1).max(0);
    let block_top = center_y - total_h / 2;
    let rot_w = d[hi].h as i32;
    let glyph_left = libm::ceilf(center_x) as i32 - rot_w / 2;
    let mut cursor_bottom = block_top + total_h;
    let mut cursor_top = block_top;
    let mut draw_one = |g: &font::Glyph| {
        let rot_h = g.w as i32;
        let glyph_top = if sunrise { cursor_bottom - rot_h } else { cursor_top };
        for src_y in 0..g.h as i32 {
            for src_x in 0..g.w as i32 {
                if g.bits[(src_y * g.w as i32 + src_x) as usize] == 0 {
                    continue;
                }
                let (rx, ry) = if sunrise {
                    (src_y, g.w as i32 - 1 - src_x)
                } else {
                    (g.h as i32 - 1 - src_y, src_x)
                };
                let px = glyph_left + rx;
                let py = glyph_top + ry;
                if px < 0 || px >= CW as i32 || py < 0 || py >= CH as i32 {
                    continue;
                }
                let idx = py as usize * CW + px as usize;
                canvas[idx] = invert_code(canvas[idx]);
            }
        }
        if sunrise {
            cursor_bottom -= rot_h + font::GLYPH_KERN;
        } else {
            cursor_top += rot_h + font::GLYPH_KERN;
        }
    };
    draw_one(&d[hi]);
    if lo != 0 {
        draw_one(&d[lo]);
    }
}

// Cosmological-cycle edge indicators (left = moon phase, right = solar year).
const NEW_MOON_REF_UNIX: i64 = 947_182_440; // 2000-01-06 18:14 UTC
const SYNODIC_SECS: f64 = 29.530_588_853 * 86400.0;
const WINTER_SOLSTICE_REF_UNIX: i64 = 1_734_772_860; // 2024-12-21 09:21 UTC
const YEAR_SECS: f64 = 365.25 * 86400.0;
const EVENT_WINDOW_SECS: f64 = 12.0 * 3600.0; // half-window for the "at extreme" bar

#[derive(Copy, Clone)]
enum CycleEventKind {
    AtMin,
    AtMax,
    Rising,
    Falling,
}

/// Fraction [0,1) of the cycle since the reference event (0 = new moon / solstice).
fn cycle_phase(now_ts: i64, ref_unix: i64, cycle_secs: f64) -> f64 {
    let p = (now_ts - ref_unix) as f64 / cycle_secs;
    p - libm::floor(p)
}

fn cycle_event_kind(phase: f64, window_phase: f64) -> CycleEventKind {
    if phase < window_phase || phase > 1.0 - window_phase {
        CycleEventKind::AtMin
    } else if libm::fabs(phase - 0.5) < window_phase {
        CycleEventKind::AtMax
    } else if phase < 0.5 {
        CycleEventKind::Rising
    } else {
        CycleEventKind::Falling
    }
}

/// Top of the 2px arrow from illuminated fraction — high (top) at full/bright.
fn phase_to_arrow_y_top(phase: f64) -> i32 {
    let illum = 0.5 * (1.0 - libm::cos(2.0 * core::f64::consts::PI * phase));
    let y = libm::round((1.0 - illum) * (CH as f64 - 2.0)) as i32;
    y.clamp(0, CH as i32 - 2)
}

/// 2px edge indicator: diagonal arrow (rising/falling) or a top/bottom bar (at
/// max/min extreme), inverting what's beneath. left_side = moon (cols 0/1), else year.
fn draw_cycle_indicator(canvas: &mut [u8], left_side: bool, kind: CycleEventKind, y_top: i32) {
    let outer: i32 = if left_side { 0 } else { CW as i32 - 1 };
    let inner: i32 = if left_side { 1 } else { CW as i32 - 2 };
    let mut put = |x: i32, y: i32| {
        if x < 0 || x >= CW as i32 || y < 0 || y >= CH as i32 {
            return;
        }
        let i = y as usize * CW + x as usize;
        canvas[i] = invert_code(canvas[i]);
    };
    match kind {
        CycleEventKind::AtMin => {
            put(outer, CH as i32 - 1);
            put(inner, CH as i32 - 1);
        }
        CycleEventKind::AtMax => {
            put(outer, 0);
            put(inner, 0);
        }
        CycleEventKind::Rising => {
            put(outer, y_top);
            put(inner, y_top + 1);
        }
        CycleEventKind::Falling => {
            put(inner, y_top);
            put(outer, y_top + 1);
        }
    }
}

/// Stamp HH:MM with the colon centred on `pivot_x` (so it lands on the now-line),
/// glyph tops at `top_y`. Mirrors tide-display's AlignChar(':') anchor.
#[allow(dead_code)] // decimal mode kept available; the clock renders dozenal
fn draw_time_hhmm(canvas: &mut [u8], hh: u32, mm: u32, pivot_x: i32, top_y: i32, color: u8) {
    let idx = [
        (hh / 10) as usize,
        (hh % 10) as usize,
        10,
        (mm / 10) as usize,
        (mm % 10) as usize,
    ];
    let d = &font::DIGITS;
    let lead = d[idx[0]].w as i32 + font::GLYPH_KERN + d[idx[1]].w as i32 + font::GLYPH_KERN;
    let mut cursor = pivot_x - lead - d[10].w as i32 / 2;
    for &i in idx.iter() {
        blit_glyph(canvas, &d[i], cursor, top_y, color);
        cursor += d[i].w as i32 + font::GLYPH_KERN;
    }
}

/// Vertical line down column `x`, skipping rows [gap_top, gap_top+gap_h) for a label.
fn draw_v_line_split(canvas: &mut [u8], x: i32, gap_top: i32, gap_h: i32, color: u8) {
    if x < 0 || x >= CW as i32 {
        return;
    }
    let gap_end = gap_top + gap_h;
    for y in 0..CH as i32 {
        if y >= gap_top && y < gap_end {
            continue;
        }
        canvas[y as usize * CW + x as usize] = color;
    }
}

/// Render a Bremerton tide chart centred on `unix` seconds into CANVAS, then pack
/// into PANEL_FB: tide-fill curve, hourly ticks (taller at local midnight), a
/// State of charge (0..=1000 permille) of a single LiPo cell from its resting
/// terminal voltage, via a piecewise-linear discharge LUT. Valid at rest/low load
/// — which is our case: we sample right before a refresh after minutes idle, so
/// the reading is near open-circuit. Below 3.27V reads empty, at/above 4.20V full.
fn batt_soc_permille(mv: u16) -> u32 {
    // (mV, permille) descending — standard 1S LiPo resting discharge curve.
    const LUT: [(u16, u16); 21] = [
        (4200, 1000), (4150, 950), (4110, 900), (4080, 850), (4020, 800),
        (3980, 750), (3950, 700), (3910, 650), (3870, 600), (3850, 550),
        (3840, 500), (3820, 450), (3800, 400), (3790, 350), (3770, 300),
        (3750, 250), (3730, 200), (3710, 150), (3690, 100), (3610, 50),
        (3270, 0),
    ];
    if mv >= LUT[0].0 {
        return 1000;
    }
    let last = LUT.len() - 1;
    if mv <= LUT[last].0 {
        return 0;
    }
    let mut i = 0;
    while i < last && mv < LUT[i + 1].0 {
        i += 1;
    }
    let (hv, hp) = LUT[i];
    let (lv, lp) = LUT[i + 1];
    lp as u32 + (hp - lp) as u32 * (mv - lv) as u32 / (hv - lv) as u32
}

async fn render_tide(unix: i64, batt_mv: u16) {
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
    // Dozenal-hour ticks (black, top + bottom edge): one per DOZENAL hour = every
    // 2 decimal hours (odd local hours dropped), local midnight taller (2px) to
    // anchor the day boundaries. Matches the Pi's dozenal mode.
    for hh in -12..=12i32 {
        let x = (CW as f32 / 2.0 + hh as f32 * 3600.0 / 86400.0 * CW as f32) as i32;
        // Skip the outer 3 columns each side — reserved for the cycle indicators.
        if x >= 3 && x < CW as i32 - 3 {
            let tick_time = unix + hh as i64 * 3600;
            let local_hour = (tick_time + tz_offset_secs(tick_time)).rem_euclid(86400) / 3600;
            if local_hour % 2 != 0 {
                continue; // odd hour — not a dozenal-hour boundary
            }
            let tick_h = if local_hour == 0 { 2 } else { 1 };
            for dy in 0..tick_h {
                canvas[dy as usize * CW + x as usize] = C_BLACK;
                canvas[(CH - 1 - dy as usize) * CW + x as usize] = C_BLACK;
            }
        }
    }
    // High/low tide markers (RED, pre-invert so they flip with day/night): a
    // full-height red line at each curve extremum with the dozenal event time set
    // OPPOSITE the curve — HIGH labels in the bottom third, LOW in the top third.
    // Slope-flip detection with flat-run midpoint, matching tide-display.
    let dh = font::DOZENAL_H as i32;
    let top3 = (CH as i32 / 3 - dh) / 2;
    let bot3 = CH as i32 - (CH as i32 / 3 + dh) / 2;
    let mut prev_dir: i8 = 0;
    let mut last_pivot = 0usize;
    for i in 1..N_SAMPLES {
        let (a, b) = (samples[i - 1], samples[i]);
        let cur: i8 = if b > a {
            1
        } else if b < a {
            -1
        } else {
            0
        };
        if cur != 0 {
            if prev_dir != 0 && cur != prev_dir {
                let is_high = prev_dir == 1;
                let t = base + ((last_pivot + i - 1) / 2) as i64 * 360;
                let frac = (t - unix) as f32 / 86400.0;
                let ex = libm::roundf(CW as f32 / 2.0 + frac * CW as f32) as i32;
                if ex >= 0 && ex < CW as i32 {
                    let ly = if is_high { bot3 } else { top3 };
                    draw_v_line_split(canvas, ex, ly - 2, dh + 4, C_RED);
                    let (lh, lm) = local_hh_mm(t);
                    let (hi, lo) = dozenal_indices(lh, lm);
                    draw_dozenal_label(canvas, hi, lo, ex, ly, C_RED);
                }
            }
            prev_dir = cur;
            last_pivot = i;
        }
    }
    // "Now" line down the centre, split to leave a gap for the dozenal time label,
    // the two Stelor symbols straddling the line.
    let nx = (CW / 2) as i32;
    let lbl_top = (CH as i32 - font::DOZENAL_H as i32) / 2;
    draw_v_line_split(canvas, nx, lbl_top - 2, font::DOZENAL_H as i32 + 4, C_BLACK);
    let (hh, mm) = local_hh_mm(unix);
    let (hi, lo) = dozenal_indices(hh, mm);
    draw_dozenal_label(canvas, hi, lo, nx, lbl_top, C_BLACK);
    // Battery gauge, top-left: outlined bar filled proportional to STATE OF CHARGE
    // (remaining capacity), not raw voltage — so each pixel is roughly equal used
    // mAh. LiPo voltage-vs-capacity is very nonlinear (flat 3.7-3.9V plateau, steep
    // ends), so a voltage bar reads full far too long then plummets.
    const BW: usize = 40;
    let level = (batt_soc_permille(batt_mv) as usize * (BW - 2) / 1000).min(BW - 2);
    for x in 2..2 + BW {
        canvas[2 * CW + x] = C_BLACK;
        canvas[6 * CW + x] = C_BLACK;
    }
    for y in 2..=6 {
        canvas[y * CW + 2] = C_BLACK;
        canvas[y * CW + (1 + BW)] = C_BLACK;
    }
    for y in 3..6 {
        for x in 3..3 + level {
            canvas[y * CW + x] = C_BLACK;
        }
    }
    // Day/night: invert every column where the sun is below the horizon, so the
    // whole night theme falls out of one pass. Yield periodically (many f64 trig
    // calls) so the WDT-pet task keeps running.
    for x in 0..CW {
        if x % 48 == 0 {
            embassy_futures::yield_now().await;
        }
        let secs = ((x as f32 - CW as f32 / 2.0) / CW as f32 * 86400.0) as i64;
        if sun_altitude_deg(unix + secs) < 0.0 {
            for y in 0..CH {
                let i = y * CW + x;
                canvas[i] = invert_code(canvas[i]);
            }
        }
    }
    // Moon-visibility line: one pixel per column riding the top row when the moon
    // is up at that column's time, the bottom row when it's down — the square-wave
    // jumps land at moonrise/moonset. Inverts like the now-line so it reads against
    // whatever day/night background is beneath it (recolors an hour tick rather than
    // vanishing, since invert_code is an involution).
    for x in 0..CW {
        if x % 48 == 0 {
            embassy_futures::yield_now().await;
        }
        let secs = ((x as f32 - CW as f32 / 2.0) / CW as f32 * 86400.0) as i64;
        let y = if moon_altitude_deg(unix + secs) > 0.0 { 0 } else { CH - 1 };
        let i = y * CW + x;
        canvas[i] = invert_code(canvas[i]);
    }
    // Sunrise/sunset rotated dozenal labels: scan the ±12h window for sun-altitude
    // zero-crossings (same 0° threshold as the day/night invert, so each label sits
    // exactly on the shading boundary), bisect to the crossing, and stamp the event
    // time rotated. Drawn after the invert so night-column pixels double-invert back
    // to day style. Rising crossing = sunrise, falling = sunset.
    let mid_y = (CH / 2) as i32;
    let mut prev_t = unix - 12 * 3600;
    let mut prev_a = sun_altitude_deg(prev_t);
    let mut t = prev_t + 600;
    let mut n = 0u32;
    while t <= unix + 12 * 3600 {
        let a = sun_altitude_deg(t);
        if (prev_a < 0.0) != (a < 0.0) {
            let (mut lo_t, mut hi_t) = (prev_t, t);
            for _ in 0..24 {
                let m = (lo_t + hi_t) / 2;
                if (sun_altitude_deg(m) < 0.0) == (prev_a < 0.0) {
                    lo_t = m;
                } else {
                    hi_t = m;
                }
            }
            let frac = (hi_t - unix) as f32 / 86400.0;
            let xf = CW as f32 / 2.0 + frac * CW as f32;
            if xf >= 0.0 && xf < CW as f32 {
                let (lh, lm) = local_hh_mm(hi_t);
                let (hi, lo) = dozenal_indices(lh, lm);
                draw_dozenal_rotated_invert(canvas, hi, lo, xf, mid_y, a > prev_a);
            }
        }
        prev_t = t;
        prev_a = a;
        t += 600;
        n += 1;
        if n % 32 == 0 {
            embassy_futures::yield_now().await;
        }
    }
    // Moon-phase (left edge) + solar-year (right edge) cycle indicators: a 2px
    // diagonal arrow whose height tracks illumination, or a top/bottom bar within
    // 12h of the extreme (full/new moon, summer/winter solstice).
    let moon_phase = cycle_phase(unix, NEW_MOON_REF_UNIX, SYNODIC_SECS);
    let year_phase = cycle_phase(unix, WINTER_SOLSTICE_REF_UNIX, YEAR_SECS);
    draw_cycle_indicator(
        canvas,
        true,
        cycle_event_kind(moon_phase, EVENT_WINDOW_SECS / SYNODIC_SECS),
        phase_to_arrow_y_top(moon_phase),
    );
    draw_cycle_indicator(
        canvas,
        false,
        cycle_event_kind(year_phase, EVENT_WINDOW_SECS / YEAR_SECS),
        phase_to_arrow_y_top(year_phase),
    );
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

/// Refresh cadence. 10 min = the design point for battery life.
const REFRESH_SECS: u64 = 600;

// Wall-clock epoch: unix seconds at uptime 0. 0 = fall back to the compile-time
// build stamp (roughly right since build ≈ flash). Set precisely over BLE via the
// time characteristic. u32 seconds is good through year 2106.
static EPOCH_BASE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

// Signalled when the clock is set, so the refresh loop abandons its stale sleep
// (scheduled against the old clock) and re-aligns to the NEW next 10-min mark —
// otherwise the first refresh after a settime fires off-mark.
static CLOCK_SET: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    (),
> = embassy_sync::signal::Signal::new();

// True while an OTA transfer is live (BEGIN..COMMIT/disconnect). The refresh loop
// skips repaints while set, so a multi-second render+panel-drive can't starve the
// BLE transfer (or stomp the bitstream LED) mid-push.
static OTA_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn now_unix() -> i64 {
    let base = EPOCH_BASE.load(core::sync::atomic::Ordering::Relaxed);
    let base = if base == 0 { BUILD_UNIX_SECS } else { base as i64 };
    base + embassy_time::Instant::now().as_secs() as i64
}

fn set_now(unix: i64) {
    let uptime = embassy_time::Instant::now().as_secs() as i64;
    // .max(1): never store 0, which is the "unset, use build stamp" sentinel.
    EPOCH_BASE.store((unix - uptime).max(1) as u32, core::sync::atomic::Ordering::Relaxed);
    CLOCK_SET.signal(()); // wake the refresh loop to re-align to the new mark
}

/// The peripherals a refresh cycle needs. Held as masters; each cycle clones them
/// (clone_unchecked) and uses them strictly sequentially (battery read fully done
/// before the panel is built), so no two live handles ever touch the same pin.
struct PanelHw {
    saadc: embassy_nrf::Peri<'static, SAADC>,
    ain: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_31>,
    ven: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_14>,
    spi: embassy_nrf::Peri<'static, TWISPI0>,
    sck: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P1_13>,
    mosi: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P1_15>,
    cs: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P1_12>,
    rst: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_15>,
    busy: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_29>,
    en: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P1_11>,
}

/// One full refresh: read the battery (P0.31/AIN7), render the chart for `now`,
/// then drive the panel (P0.31 reused as DC — sequential, so sound) and power it
/// off. Returns the battery reading. This is BOTH the boot self-test and the loop
/// body — if it survives the self-test, the loop can't crash.
async fn do_refresh(hw: &PanelHw, now: i64) -> (u16, u16) {
    led(true); // dim "busy" glow while we read battery + render + drive the panel
    let batt = read_battery(
        unsafe { hw.saadc.clone_unchecked() },
        unsafe { hw.ain.clone_unchecked() },
        unsafe { hw.ven.clone_unchecked() },
    )
    .await;
    let mut sc = spim::Config::default();
    sc.frequency = spim::Frequency::M8;
    let spi = Spim::new_txonly(
        unsafe { hw.spi.clone_unchecked() },
        Irqs,
        unsafe { hw.sck.clone_unchecked() },
        unsafe { hw.mosi.clone_unchecked() },
        sc,
    );
    let mut panel = Panel {
        spi,
        cs: Output::new(unsafe { hw.cs.clone_unchecked() }, Level::High, OutputDrive::Standard),
        dc: Output::new(unsafe { hw.ain.clone_unchecked() }, Level::Low, OutputDrive::Standard),
        rst: Output::new(unsafe { hw.rst.clone_unchecked() }, Level::High, OutputDrive::Standard),
        busy: Input::new(unsafe { hw.busy.clone_unchecked() }, Pull::None),
        en: Output::new(unsafe { hw.en.clone_unchecked() }, Level::Low, OutputDrive::Standard),
    };
    render_tide(now, batt.0).await;
    panel.init().await;
    let fb = unsafe { &*core::ptr::addr_of!(PANEL_FB) };
    panel.push(fb).await;
    led(false); // done — back to fully off
    batt
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }

    let p = embassy_nrf::init(Default::default());
    park_rgb_leds(); // force onboard RGB LED off (drive high) — see fn comment
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
    // LFCLK from the on-board 32.768 kHz CRYSTAL (~±20-50 ppm ≈ 2-4 s/day) instead
    // of the internal RC (~±500 ppm ≈ 40 s/day) — this is the dominant drift lever,
    // and embassy-time's RTC1 rides the same LFCLK so the whole clock benefits. If
    // the crystal were unpopulated it wouldn't start; skip_wait=false blocks, the
    // WDT isn't petted during MPSL init, so it just auto-reverts. rc_ctiv=0 (no RC
    // calibration needed for a crystal). accuracy_ppm=50 = safe upper bound for BLE
    // sleep-clock timing.
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 50,
        skip_wait_lfclk_started: false,
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

    // OTA flash: the app's own Nvmc behind a Mutex, shared with per-connection
    // FirmwareUpdaters. from_linkerfile reads the __bootloader_dfu/state symbols.
    let nvmc = Mutex::<NoopRawMutex, _>::new(BlockingAsync::new(Nvmc::new(p.NVMC)));

    // Hold the battery + panel peripherals as masters; do_refresh clones them each
    // cycle. The FIRST refresh is the boot self-test (battery -> render -> panel).
    // If it hangs/crashes, mark_booted below is never reached -> WDT -> auto-revert.
    let hw = PanelHw {
        saadc: p.SAADC,
        ain: p.P0_31,
        ven: p.P0_14,
        spi: p.TWISPI0,
        sck: p.P1_13,
        mosi: p.P1_15,
        cs: p.P1_12,
        rst: p.P0_15,
        busy: p.P0_29,
        en: p.P1_11,
    };
    let batt = do_refresh(&hw, now_unix()).await;
    info!("[boot] self-test refresh OK, batt {} mV", batt.0);

    // Confirm the boot ONLY after the self-test refresh succeeded.
    {
        let mut aligned = AlignedBuffer([0u8; 4]);
        let cfg = FirmwareUpdaterConfig::from_linkerfile(&nvmc, &nvmc);
        let mut updater = FirmwareUpdater::new(cfg, &mut aligned.0);
        if let Ok(State::Revert) = updater.get_state().await {
            info!("[ota] previous update was REVERTED by the bootloader");
        }
        let _ = updater.mark_booted().await;
    }

    run(sdc, &nvmc, &hw, batt).await;
}

async fn run<C, F>(controller: C, nvmc: &Mutex<NoopRawMutex, F>, hw: &PanelHw, batt: (u16, u16))
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

    let set_batt = |b: (u16, u16), count: u16| {
        let mut bch = [0u8; 6];
        bch[0..2].copy_from_slice(&b.0.to_le_bytes());
        bch[2..4].copy_from_slice(&b.1.to_le_bytes());
        bch[4..6].copy_from_slice(&count.to_le_bytes());
        let _ = server.set(&server.ota.battery, &bch);
    };
    set_batt(batt, 0);

    let _ = join3(
        ble_task(runner),
        // Advertise + serve OTA, forever.
        async {
            loop {
                match advertise(ADV_NAME, &mut peripheral, &server).await {
                    Ok(conn) => {
                        // Fresh updater per connection so write_firmware's last-erased
                        // tracking resets (stale tracking across pushes corrupts DFU).
                        let mut aligned = AlignedBuffer([0u8; 4]);
                        let cfg = FirmwareUpdaterConfig::from_linkerfile(nvmc, nvmc);
                        let mut updater = FirmwareUpdater::new(cfg, &mut aligned.0);
                        ota_session(&server, &conn, &mut updater).await;
                    }
                    Err(_) => Timer::after(Duration::from_millis(500)).await,
                }
            }
        },
        // The CLOCK: refresh the panel every REFRESH_SECS for the current time.
        // do_refresh here is identical to the boot self-test, so it can't crash if
        // the self-test passed. Reads the battery each cycle (the before-refresh
        // idea) and bumps the count so a BLE reader can see it ticking.
        async {
            let mut count = 0u16;
            loop {
                // Repaint on the :05/:15/:25… boundaries, not :00/:10/:20. The
                // dozenal readout rounds to the NEAREST 10-min mark, so its symbol
                // flips at the half-step (:05) — repainting there keeps the panel
                // always showing the current symbol (repainting on :00/:10 would
                // show a stale symbol for the first 5 min of each interval). Drift
                // still shows as the repaint creeping off the boundary. If the clock
                // is SET mid-sleep, CLOCK_SET bails us out to recompute.
                let now = now_unix();
                let phase = now.rem_euclid(REFRESH_SECS as i64); // secs past the :00 mark
                let to_mark = if phase < 300 { 300 - phase } else { 900 - phase };
                match embassy_futures::select::select(
                    Timer::after(Duration::from_secs(to_mark as u64)),
                    CLOCK_SET.wait(),
                )
                .await
                {
                    embassy_futures::select::Either::First(_) => {
                        // Skip the repaint if an OTA is streaming — a multi-second
                        // render+panel-drive would starve the transfer and stomp the
                        // bitstream LED. The swap-boot self-test repaints anyway.
                        if !OTA_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
                            let b = do_refresh(hw, now_unix()).await;
                            count = count.wrapping_add(1);
                            set_batt(b, count);
                        }
                    }
                    // Clock was just set — loop to recompute to_mark against it.
                    embassy_futures::select::Either::Second(_) => {}
                }
            }
        },
    )
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
    let time_h = server.ota.time;

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
            GattConnectionEvent::Disconnected { .. } => {
                OTA_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed); // transfer over — resume refreshes
                break;
            }
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
                            // Bitstream monitor: clock this packet's first 32-bit word
                            // out the LED, ~1 ms/bit (MSB first), spread across the idle
                            // inter-packet gap — a real ~1 kHz bit display, not one
                            // sample/packet. img_recv is already updated so flow control
                            // isn't paced; each await yields to BLE; and the ~32 ms fits
                            // inside the ~57 ms packet period so throughput holds. 1 =
                            // dim green (float), 0 = dark.
                            if clen >= 4 {
                                let word =
                                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                                for b in (0..32).rev() {
                                    led((word >> b) & 1 == 1);
                                    Timer::after(Duration::from_millis(1)).await;
                                }
                            }
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
                                OTA_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed); // pause refreshes for the transfer
                                led(true); // initial glow; the per-chunk bitstream flicker takes over as data flows
                            } else if cn >= 1 && cb[0] == 0x02 {
                                // Reply to COMMIT FAST (status=verifying); do the heavy
                                // verify AFTER the reply so the host's write doesn't time out.
                                set_status(2, img_recv);
                                led(false); // transfer done (verify/swap or reject next)
                                commit_pending = true;
                            }
                            event.accept_unprocessed()
                        } else if event.handle() == time_h.handle {
                            // Set the wall clock: 8-byte little-endian unix seconds.
                            let mut tb = [0u8; 8];
                            let mut tn = 0usize;
                            event.with_data(|_o, b| {
                                tn = b.len().min(8);
                                tb[..tn].copy_from_slice(&b[..tn]);
                            });
                            if tn >= 8 {
                                let unix = i64::from_le_bytes(tb);
                                set_now(unix);
                                info!("[time] set unix={}", unix);
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
