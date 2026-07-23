//! Transfer round-trip test against the tideglyph device: connect over BLE, stream a deterministic payload in chunks to the data characteristic, COMMIT, read back the device's BLAKE3, and compare with the locally computed hash. PASS proves the whole radio + chunking + on-device hashing path — the skeleton the signed-OTA push builds on.
//!
//! Usage: tideglyph-push [payload_bytes]   (default 4096)

use anyhow::{anyhow, Context, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::Manager;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CTRL: Uuid = Uuid::from_u128(0xb5f90002_2d5a_4f3c_9b1a_1d2e3f405060);
const DATA: Uuid = Uuid::from_u128(0xb5f90003_2d5a_4f3c_9b1a_1d2e3f405060);
const HASH: Uuid = Uuid::from_u128(0xb5f90004_2d5a_4f3c_9b1a_1d2e3f405060);
const COUNT: Uuid = Uuid::from_u128(0xb5f90005_2d5a_4f3c_9b1a_1d2e3f405060);

const BEGIN: u8 = 0x01;
const COMMIT: u8 = 0x02;
/// Conservative chunk size: fits any ATT MTU (23 → 20 usable). Bump after MTU negotiation is wired up.
const CHUNK: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    let size: usize = std::env::args().nth(1).map(|s| s.parse()).transpose()?.unwrap_or(4096);

    // Deterministic test payload.
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected = blake3::hash(&payload);

    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().next().ok_or_else(|| anyhow!("no BLE adapter"))?;

    println!("scanning for tideglyph…");
    central.start_scan(ScanFilter::default()).await?;
    let device = 'found: {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in central.peripherals().await? {
                if let Ok(Some(props)) = p.properties().await {
                    if props.local_name.as_deref() == Some("tideglyph") {
                        break 'found p;
                    }
                }
            }
        }
        return Err(anyhow!("tideglyph not found in 30s of scanning"));
    };
    central.stop_scan().await.ok();

    println!("connecting…");
    device.connect().await.context("connect")?;
    device.discover_services().await.context("discover")?;

    let chars = device.characteristics();
    let find = |u: Uuid| chars.iter().find(|c| c.uuid == u).cloned().ok_or_else(|| anyhow!("characteristic {u} not found"));
    let (ctrl, data, hash, count) = (find(CTRL)?, find(DATA)?, find(HASH)?, find(COUNT)?);

    println!("BEGIN + streaming {size} bytes in {CHUNK}-byte chunks…");
    device.write(&ctrl, &[BEGIN], WriteType::WithResponse).await?;

    let t0 = Instant::now();
    for chunk in payload.chunks(CHUNK) {
        device.write(&data, chunk, WriteType::WithResponse).await?;
    }
    let elapsed = t0.elapsed();

    device.write(&ctrl, &[COMMIT], WriteType::WithResponse).await?;

    let got_count = device.read(&count).await?;
    let got_hash = device.read(&hash).await?;
    device.disconnect().await.ok();

    let received = u32::from_le_bytes(got_count[..4].try_into()?);
    println!("device received {received} bytes in {:.2}s ({:.0} B/s)", elapsed.as_secs_f64(), size as f64 / elapsed.as_secs_f64());
    println!("device hash: {}", hex(&got_hash));
    println!("local  hash: {}", hex(expected.as_bytes()));

    if received == size as u32 && got_hash.as_slice() == expected.as_bytes() {
        println!("PASS — radio + chunking + on-device BLAKE3 all agree");
        Ok(())
    } else {
        Err(anyhow!("FAIL — count or hash mismatch"))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
