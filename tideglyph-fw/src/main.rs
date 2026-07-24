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
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::peripherals::RNG;
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

// Global allocator — vsf's verify path uses alloc (Vec/String while parsing the
// signed manifest). 16 KB is ample for a few-hundred-byte manifest doc.
#[global_allocator]
static HEAP: embedded_alloc::LlffHeap = embedded_alloc::LlffHeap::empty();
const HEAP_SIZE: usize = 16 * 1024;
static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// This build's Eagle-time stamp — the downgrade FLOOR (see build.rs). An OTA
/// manifest is accepted only if its stamp strictly exceeds this.
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
    EGU0_SWI0 => nrf_sdc::mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_sdc::mpsl::ClockInterruptHandler;
    RADIO => nrf_sdc::mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_sdc::mpsl::HighPrioInterruptHandler;
});

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

// GATT server: transfer-test service.
#[gatt_server]
struct Server {
    xfer: XferService,
}

#[gatt_service(uuid = "b5f90001-2d5a-4f3c-9b1a-1d2e3f405060")]
struct XferService {
    #[characteristic(uuid = "b5f90002-2d5a-4f3c-9b1a-1d2e3f405060", write)]
    ctrl: u8,
    #[characteristic(uuid = "b5f90003-2d5a-4f3c-9b1a-1d2e3f405060", write, write_without_response, value = [0u8; 244])]
    data: [u8; 244],
    #[characteristic(uuid = "b5f90004-2d5a-4f3c-9b1a-1d2e3f405060", read)]
    hash: [u8; 32],
    #[characteristic(uuid = "b5f90005-2d5a-4f3c-9b1a-1d2e3f405060", read)]
    count: [u8; 4],
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

    // Self-test proxy: reaching here means embassy + MPSL + the BLE controller
    // all came up. Confirm this boot so that if the bootloader just swapped in a
    // new image, the swap sticks (BOOT_MAGIC) instead of reverting on the next
    // power-cycle. Harmless no-op on a normal (non-swapped) boot. If a bad image
    // crashes before this point, mark_booted is never reached -> WDT -> revert.
    {
        let nvmc = Mutex::<NoopRawMutex, _>::new(BlockingAsync::new(Nvmc::new(p.NVMC)));
        let fw_config = FirmwareUpdaterConfig::from_linkerfile(&nvmc, &nvmc);
        let mut aligned = AlignedBuffer([0u8; 4]);
        let mut updater = FirmwareUpdater::new(fw_config, &mut aligned.0);
        if let Ok(State::Revert) = updater.get_state().await {
            info!("[ota] previous update was REVERTED by the bootloader");
        }
        let _ = updater.mark_booted().await;
    }

    run(sdc).await;
}

async fn run<C: Controller>(controller: C) {
    let address: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let server = unwrap!(Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: "tideglyph",
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    })));

    let _ = join(ble_task(runner), async {
        loop {
            led(false); // advertising: LED off
            match advertise("tideglyph", &mut peripheral, &server).await {
                Ok(conn) => {
                    led(true); // connected: LED on
                    let _ = xfer_task(&server, &conn).await;
                }
                Err(_) => {
                    // Brief triple-flutter on advertise error, then retry.
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

/// Handle the transfer service until the connection closes: BEGIN resets the hasher, data writes feed it, COMMIT finalizes into the hash characteristic.
async fn xfer_task<P: PacketPool>(server: &Server<'_>, conn: &GattConnection<'_, '_, P>) -> Result<(), Error> {
    let ctrl = server.xfer.ctrl;
    let data = server.xfer.data;
    let hash = server.xfer.hash;
    let count = server.xfer.count;

    let mut hasher = blake3::Hasher::new();
    let mut total: u32 = 0;
    let mut digest = [0u8; 32];

    let _reason = loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break reason,
            GattConnectionEvent::Gatt { event } => {
                let reply = match event {
                    // Reads are answered by the server from the attribute table; handlers below keep hash/count stored there up to date. (Do NOT use accept_unprocessed(&data) for reads — at this trouble rev it slices the response by MTU without bounding by data length and panics for short values.)
                    GattEvent::Read(event) => event.accept(),
                    GattEvent::Write(event) => {
                        if event.handle() == data.handle {
                            event.with_data(|_offset, bytes| {
                                hasher.update(bytes);
                                total = total.wrapping_add(bytes.len() as u32);
                            });
                            event.accept_unprocessed()
                        } else if event.handle() == ctrl.handle {
                            let mut op = 0u8;
                            event.with_data(|_offset, bytes| {
                                if !bytes.is_empty() {
                                    op = bytes[0];
                                }
                            });
                            match op {
                                0x01 => {
                                    hasher.reset();
                                    total = 0;
                                    digest = [0u8; 32];
                                    let _ = server.set(&hash, &digest);
                                    let _ = server.set(&count, &[0u8; 4]);
                                    info!("[xfer] BEGIN");
                                }
                                0x02 => {
                                    digest = *hasher.finalize().as_bytes();
                                    let _ = server.set(&hash, &digest);
                                    let _ = server.set(&count, &total.to_le_bytes());
                                    info!("[xfer] COMMIT: {} bytes", total);
                                }
                                _ => {}
                            }
                            event.accept_unprocessed()
                        } else {
                            event.accept()
                        }
                    }
                    _ => event.accept(),
                };
                match reply {
                    Ok(reply) => reply.send().await,
                    Err(_) => {}
                }
            }
            _ => {}
        }
    };
    Ok(())
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
