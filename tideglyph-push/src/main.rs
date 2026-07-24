//! Host-side OTA pusher for the tideglyph device.
//!
//!   tideglyph-push manifest <image.bin>   build + self-verify the manifest (no device)
//!   tideglyph-push push     <image.bin>   OTA the image over BLE (build manifest, stream, commit)
//!
//! Protocol (GATT, base b5f9xxxx-2d5a-4f3c-9b1a-1d2e3f405060), shared with the firmware:
//!   ctrl  (0002, write):  BEGIN = [0x01, image_len u32 LE, manifest_len u16 LE]; COMMIT = [0x02]
//!   data  (0003, write-without-response): image bytes (image_len) then manifest bytes (manifest_len)
//!   status(0004, read):   [state u8, verdict u8, received u32 LE]
//!     verdict: 0 idle, 1 receiving, 2 verifying, 3 ACCEPTED (device will swap+reset),
//!              10 bad-magic/len, 11 stale-stamp, 12 bad-mac, 13 decode-fail

mod manifest;

use anyhow::{anyhow, Context, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::Manager;
use std::time::Duration;
use uuid::Uuid;

const CTRL: Uuid = Uuid::from_u128(0xb5f90002_2d5a_4f3c_9b1a_1d2e3f405060);
const DATA: Uuid = Uuid::from_u128(0xb5f90003_2d5a_4f3c_9b1a_1d2e3f405060);
const STATUS: Uuid = Uuid::from_u128(0xb5f90004_2d5a_4f3c_9b1a_1d2e3f405060);

const BEGIN: u8 = 0x01;
const COMMIT: u8 = 0x02;
const CHUNK: usize = 240; // fits a negotiated ATT MTU; write-without-response for speed

fn ota_key() -> Result<[u8; 32]> {
    let path = std::env::var("OTA_KEY_FILE")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../ota_key.bin").to_string());
    let bytes = std::fs::read(&path).with_context(|| format!("read OTA key {path}"))?;
    bytes.as_slice().try_into().map_err(|_| anyhow!("OTA key must be 32 bytes, got {}", bytes.len()))
}

fn now_stamp() -> i64 {
    vsf::eagle_time_oscillations()
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let image_path = args.next();

    match cmd.as_str() {
        "manifest" => cmd_manifest(&image_path.context("usage: manifest <image.bin>")?),
        "push" => cmd_push(&image_path.context("usage: push <image.bin>")?).await,
        _ => {
            eprintln!("usage: tideglyph-push <manifest|push> <image.bin>");
            std::process::exit(2);
        }
    }
}

fn cmd_manifest(image_path: &str) -> Result<()> {
    let key = ota_key()?;
    let image = std::fs::read(image_path).with_context(|| format!("read {image_path}"))?;
    let stamp = now_stamp();
    let manifest = manifest::build_manifest(&key, stamp, &image)?;
    let vstamp = manifest::verify_roundtrip(&key, &manifest, &image)?;
    assert_eq!(stamp, vstamp);
    println!("image        {} bytes", image.len());
    println!("stamp        {stamp} (eagle osc)");
    println!("manifest     {} bytes", manifest.len());
    println!("SELF-CHECK   OK — manifest parses back, MAC matches, size matches");
    Ok(())
}

async fn cmd_push(image_path: &str) -> Result<()> {
    let key = ota_key()?;
    let image = std::fs::read(image_path).with_context(|| format!("read {image_path}"))?;
    let stamp = now_stamp();
    let manifest = manifest::build_manifest(&key, stamp, &image)?;
    manifest::verify_roundtrip(&key, &manifest, &image)?; // never push what we can't verify
    println!("image {} bytes, manifest {} bytes, stamp {stamp}", image.len(), manifest.len());

    let central = Manager::new().await?.adapters().await?.into_iter().next().context("no BLE adapter")?;
    println!("scanning for tideglyph…");
    central.start_scan(ScanFilter::default()).await?;
    let dev = 'f: {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in central.peripherals().await? {
                if let Ok(Some(pr)) = p.properties().await {
                    if pr.local_name.as_deref() == Some("tideglyph") {
                        break 'f p;
                    }
                }
            }
        }
        return Err(anyhow!("tideglyph not found"));
    };
    central.stop_scan().await.ok();
    dev.connect().await.context("connect")?;
    dev.discover_services().await.context("discover")?;
    let chars = dev.characteristics();
    let find = |u: Uuid| chars.iter().find(|c| c.uuid == u).cloned().ok_or_else(|| anyhow!("char {u} missing"));
    let (ctrl, data, status) = (find(CTRL)?, find(DATA)?, find(STATUS)?);

    // BEGIN
    let mut begin = vec![BEGIN];
    begin.extend_from_slice(&(image.len() as u32).to_le_bytes());
    begin.extend_from_slice(&(manifest.len() as u16).to_le_bytes());
    dev.write(&ctrl, &begin, WriteType::WithResponse).await?;

    // stream image, then manifest, over the one data characteristic
    let t0 = std::time::Instant::now();
    let mut sent = 0usize;
    let total = image.len() + manifest.len();
    for byte_stream in [image.as_slice(), manifest.as_slice()] {
        for chunk in byte_stream.chunks(CHUNK) {
            dev.write(&data, chunk, WriteType::WithoutResponse).await?;
            sent += chunk.len();
            if sent % (CHUNK * 40) < CHUNK {
                print!("\r  {sent}/{total} bytes ({:.0} B/s)", sent as f64 / t0.elapsed().as_secs_f64());
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
        }
    }
    println!("\r  {sent}/{total} bytes in {:.1}s ({:.0} B/s)      ", t0.elapsed().as_secs_f64(), sent as f64 / t0.elapsed().as_secs_f64());

    // COMMIT → device verifies, then either sets ACCEPTED + resets (connection drops) or reports an error
    dev.write(&ctrl, &[COMMIT], WriteType::WithResponse).await?;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        match dev.read(&status).await {
            Ok(s) if s.len() >= 6 => {
                let verdict = s[1];
                let recvd = u32::from_le_bytes(s[2..6].try_into().unwrap());
                match verdict {
                    3 => {
                        println!("ACCEPTED — device staged + verified {recvd} bytes; swapping + rebooting");
                        return Ok(());
                    }
                    2 | 1 => continue, // still verifying/receiving
                    e => return Err(anyhow!("device REJECTED update: verdict {e} (received {recvd})")),
                }
            }
            // connection dropped right after COMMIT = device accepted and reset into the swap
            Err(_) => {
                println!("connection dropped after COMMIT — device likely accepted + rebooting into swap");
                return Ok(());
            }
            _ => continue,
        }
    }
    Err(anyhow!("no verdict from device after COMMIT"))
}
