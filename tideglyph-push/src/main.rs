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
const BATTERY: Uuid = Uuid::from_u128(0xb5f90005_2d5a_4f3c_9b1a_1d2e3f405060);
const TIME: Uuid = Uuid::from_u128(0xb5f90006_2d5a_4f3c_9b1a_1d2e3f405060);

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
    // OTA_STAMP overrides for testing the downgrade floor (e.g. OTA_STAMP=1).
    if let Ok(s) = std::env::var("OTA_STAMP") {
        if let Ok(v) = s.parse::<i64>() {
            return v;
        }
    }
    vsf::eagle_time_oscillations()
}

fn target_name() -> String {
    std::env::var("OTA_NAME").unwrap_or_else(|_| "tideglyph".to_string())
}

/// Read the image and pad to a 4-byte boundary with 0xFF (erased-flash value) so
/// every streamed DFU write lands aligned (flash WRITE_SIZE = 4). The MAC + size
/// cover the padded image; the device writes + MACs the same padded bytes.
fn read_image_padded(path: &str) -> Result<Vec<u8>> {
    let mut image = std::fs::read(path).with_context(|| format!("read {path}"))?;
    while image.len() % 4 != 0 {
        image.push(0xFF);
    }
    Ok(image)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let image_path = args.next();

    match cmd.as_str() {
        "manifest" => cmd_manifest(&image_path.context("usage: manifest <image.bin>")?),
        "push" => cmd_push(&image_path.context("usage: push <image.bin>")?).await,
        "battery" => cmd_battery().await,
        "settime" => cmd_settime().await,
        _ => {
            eprintln!("usage: tideglyph-push <manifest|push|battery|settime> [image.bin]");
            std::process::exit(2);
        }
    }
}

async fn cmd_battery() -> Result<()> {
    let central = Manager::new().await?.adapters().await?.into_iter().next().context("no BLE adapter")?;
    let name = target_name();
    println!("scanning for {name}…");
    central.start_scan(ScanFilter::default()).await?;
    let dev = 'f: {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in central.peripherals().await? {
                if let Ok(Some(pr)) = p.properties().await {
                    if pr.local_name.as_deref() == Some(name.as_str()) {
                        break 'f p;
                    }
                }
            }
        }
        return Err(anyhow!("{name} not found"));
    };
    central.stop_scan().await.ok();
    dev.connect().await?;
    dev.discover_services().await?;
    let ch = dev.characteristics().into_iter().find(|c| c.uuid == BATTERY).context("no battery char")?;
    let v = dev.read(&ch).await?;
    dev.disconnect().await.ok();
    if v.len() >= 6 {
        let mv = u16::from_le_bytes([v[0], v[1]]);
        let raw = u16::from_le_bytes([v[2], v[3]]);
        let count = u16::from_le_bytes([v[4], v[5]]);
        println!("battery: {mv} mV  (raw {raw})  refresh_count {count}");
    } else if v.len() >= 4 {
        let mv = u16::from_le_bytes([v[0], v[1]]);
        let raw = u16::from_le_bytes([v[2], v[3]]);
        println!("battery: {mv} mV  (raw {raw})");
    } else {
        println!("battery char returned {} bytes: {v:?}", v.len());
    }
    Ok(())
}

async fn cmd_settime() -> Result<()> {
    let central = Manager::new().await?.adapters().await?.into_iter().next().context("no BLE adapter")?;
    let name = target_name();
    println!("scanning for {name}…");
    central.start_scan(ScanFilter::default()).await?;
    let dev = 'f: {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in central.peripherals().await? {
                if let Ok(Some(pr)) = p.properties().await {
                    if pr.local_name.as_deref() == Some(name.as_str()) {
                        break 'f p;
                    }
                }
            }
        }
        return Err(anyhow!("{name} not found"));
    };
    central.stop_scan().await.ok();
    dev.connect().await?;
    dev.discover_services().await?;
    let ch = dev.characteristics().into_iter().find(|c| c.uuid == TIME).context("no time char")?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    dev.write(&ch, &now.to_le_bytes(), WriteType::WithResponse).await?;
    dev.disconnect().await.ok();
    println!("set device clock to unix {now}");
    Ok(())
}

fn cmd_manifest(image_path: &str) -> Result<()> {
    let key = ota_key()?;
    let image = read_image_padded(image_path)?;
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
    let image = read_image_padded(image_path)?;
    let stamp = now_stamp();
    let manifest = manifest::build_manifest(&key, stamp, &image)?;
    manifest::verify_roundtrip(&key, &manifest, &image)?; // never push what we can't verify
    let name = target_name();
    println!("image {} bytes, manifest {} bytes, stamp {stamp}, target {name}", image.len(), manifest.len());

    let central = Manager::new().await?.adapters().await?.into_iter().next().context("no BLE adapter")?;
    println!("scanning for {name}…");
    central.start_scan(ScanFilter::default()).await?;
    let dev = 'f: {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in central.peripherals().await? {
                if let Ok(Some(pr)) = p.properties().await {
                    if pr.local_name.as_deref() == Some(name.as_str()) {
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

    // Stream the IMAGE with write-without-response (fast) but flow-controlled:
    // never let the device fall more than WINDOW bytes behind us (read its
    // `received` counter as backpressure). Bounded in-flight -> BlueZ doesn't
    // drop, and the fast path stays fast. The tiny manifest goes WithResponse.
    let read_received = |s: &[u8]| -> usize {
        if s.len() >= 6 { u32::from_le_bytes(s[2..6].try_into().unwrap()) as usize } else { 0 }
    };
    const WINDOW: usize = 8192;
    let t0 = std::time::Instant::now();
    let mut sent = 0usize;
    for chunk in image.chunks(CHUNK) {
        dev.write(&data, chunk, WriteType::WithoutResponse).await?;
        sent += chunk.len();
        if sent % 4096 < CHUNK {
            // backpressure: wait until the device is within WINDOW of us
            loop {
                let recv = read_received(&dev.read(&status).await?);
                if sent.saturating_sub(recv) <= WINDOW {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            print!("\r  {sent}/{} image bytes ({:.0} B/s)", image.len(), sent as f64 / t0.elapsed().as_secs_f64());
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    }
    // ensure every image byte actually landed (WNR could otherwise silently drop)
    let mut ok = false;
    for _ in 0..200 {
        if read_received(&dev.read(&status).await?) >= image.len() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if !ok {
        return Err(anyhow!("device did not receive the full image (WNR drop) — retry or use reliable mode"));
    }
    println!("\r  {} image bytes in {:.1}s ({:.0} B/s)      ", image.len(), t0.elapsed().as_secs_f64(), image.len() as f64 / t0.elapsed().as_secs_f64());
    // manifest reliably (small)
    for chunk in manifest.chunks(CHUNK) {
        dev.write(&data, chunk, WriteType::WithResponse).await?;
    }

    // COMMIT → device replies fast (status=verifying) then verifies; tolerate the
    // write result and poll status for the real verdict.
    let _ = dev.write(&ctrl, &[COMMIT], WriteType::WithResponse).await;
    for _ in 0..40 {
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
