//! tideglyph BLE bring-up: advertise as "tideglyph" with staged LED diagnostics on D9 = P1.14 (user-soldered LED, active high). Stages: 1 blink = embassy init OK, 2 = MPSL up, 3 = SDC built, 4 = advertising, then a short blip every 2 s while the BLE runner is alive.

#![no_std]
#![no_main]

use bt_hci::controller::ControllerCmdSync;
use bt_hci::cmd::le::*;
use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_nrf::mode::Async;
use embassy_nrf::peripherals::RNG;
use embassy_nrf::{bind_interrupts, rng};
use embassy_time::{Duration, Timer};
use nrf_sdc::mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, mpsl};
use static_cell::StaticCell;
use trouble_host::prelude::*;
use {defmt_rtt as _, panic_probe as _};

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

// Runs before statics init, before any interrupt can fire. The UF2 bootloader's MBR forwards exceptions to the BOOTLOADER's vector table for a bare app, so the first interrupt (embassy's time driver) faults → nRF52 lockup → auto-reset → the observed ~1.5 Hz crash-boot loop. Pointing VTOR at our own vector table (FLASH ORIGIN 0x1000) routes exceptions directly to us.
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    (0xE000_ED08 as *mut u32).write_volatile(0x0000_1000);
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

fn build_sdc<'d, const N: usize>(
    p: nrf_sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<Async>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<nrf_sdc::SoftdeviceController<'d>, nrf_sdc::Error> {
    sdc::Builder::new()?.support_adv().build(p, rng, mpsl, mem)
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());
    led_init();
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
    // Quick pre-marker: rng + peripherals constructed (a lone 60 ms blip before the build attempt).
    led(true);
    Timer::after(Duration::from_millis(60)).await;
    led(false);
    Timer::after(Duration::from_millis(300)).await;

    let mut sdc_mem = sdc::Mem::<4096>::new();
    let sdc = match build_sdc(sdc_p, &mut rng, mpsl, &mut sdc_mem) {
        Ok(s) => s,
        Err(e) => {
            // Blink the failure signature forever: 5 = ENOMEM (pool too small), 4 = EINVAL, 6 = EPERM, 3 = anything else.
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

    run(sdc).await;
}

async fn run<C>(controller: C)
where
    C: Controller
        + for<'t> ControllerCmdSync<LeSetExtAdvData<'t>>
        + ControllerCmdSync<LeClearAdvSets>
        + ControllerCmdSync<LeSetExtAdvParams>
        + ControllerCmdSync<LeSetAdvSetRandomAddr>
        + ControllerCmdSync<LeReadNumberOfSupportedAdvSets>
        + for<'t> ControllerCmdSync<LeSetExtAdvEnable<'t>>
        + for<'t> ControllerCmdSync<LeSetExtScanResponseData<'t>>,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let mut resources: HostResources<DefaultPacketPool, 0, 0> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let mut adv_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::CompleteLocalName(b"tideglyph"),
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
        ],
        &mut adv_data[..],
    )
    .unwrap();

    let _ = join(runner.run(), async {
        loop {
            let mut params = AdvertisementParameters::default();
            params.interval_min = Duration::from_millis(100);
            params.interval_max = Duration::from_millis(100);
            let _advertiser = peripheral
                .advertise(
                    &params,
                    Advertisement::NonconnectableScannableUndirected {
                        adv_data: &adv_data[..len],
                        scan_data: &[],
                    },
                )
                .await
                .unwrap();
            stage(4).await; // advertising live
            loop {
                // Heartbeat blip: BLE runner alive.
                led(true);
                Timer::after(Duration::from_millis(60)).await;
                led(false);
                Timer::after(Duration::from_millis(1940)).await;
            }
        }
    })
    .await;
}
