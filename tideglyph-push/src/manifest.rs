//! OTA manifest: a VSF document that authenticates a firmware image with a
//! keyed-BLAKE3 MAC (256-bit shared secret) + an Eagle-time stamp. This is the
//! exact format the device's `parse_manifest` + COMMIT gate consume — keep the
//! two in lockstep.

use anyhow::{anyhow, Context, Result};
use vsf::types::{EtType, VsfType};

/// Domain separation tag bound into the MAC — must byte-match the firmware's `OTA_DOMAIN`.
pub const OTA_DOMAIN: &[u8] = b"tideglyph-ota-v1";

/// keyed_hash(OTA_KEY, OTA_DOMAIN ‖ stamp_le ‖ image_len_le ‖ image). Binding the
/// stamp + length into the MAC stops a valid (image,mac) pair being replayed
/// under a fresh stamp to beat the downgrade floor.
pub fn ota_mac(key: &[u8; 32], stamp: i64, image: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(OTA_DOMAIN);
    h.update(&stamp.to_le_bytes());
    h.update(&(image.len() as u32).to_le_bytes());
    h.update(image);
    *h.finalize().as_bytes()
}

/// Build the signed(=MAC'd) VSF manifest for `image`. Returns the manifest bytes.
pub fn build_manifest(key: &[u8; 32], stamp: i64, image: &[u8]) -> Result<Vec<u8>> {
    let mac = ota_mac(key, stamp, image);
    vsf::vsf_builder::VsfBuilder::new()
        .creation_time_oscillations(stamp)
        .add_section(
            "firmware.tideglyph",
            vec![
                ("size".to_string(), VsfType::z(image.len())),
                ("mac".to_string(), VsfType::gH(mac.to_vec())),
            ],
        )
        .build()
        .map_err(|e| anyhow!("vsf build: {e}"))
}

/// Parse a manifest back the way the device does — used as a self-check that
/// host and device agree on the format + MAC before touching hardware.
pub fn verify_roundtrip(key: &[u8; 32], manifest: &[u8], image: &[u8]) -> Result<i64> {
    let (header, end) =
        vsf::verification::read_verified(manifest, None).map_err(|e| anyhow!("read_verified: {e}"))?;
    let stamp = match &header.creation_time {
        Some(VsfType::e(EtType::e6(o))) => *o,
        _ => return Err(anyhow!("no e6 stamp")),
    };
    let sections = header.sections(manifest, end).map_err(|e| anyhow!("sections: {e}"))?;
    let sec = sections
        .iter()
        .find(|s| s.name == "firmware.tideglyph")
        .context("no firmware.tideglyph section")?;
    let size = sec
        .get_fields("size")
        .first()
        .and_then(|f| f.values.first())
        .and_then(|v| match v {
            VsfType::z(n) => Some(*n),
            _ => None,
        })
        .context("no size field")?;
    let mac: Vec<u8> = sec
        .get_fields("mac")
        .first()
        .and_then(|f| f.values.first())
        .and_then(|v| match v {
            VsfType::gH(h) => Some(h.clone()),
            _ => None,
        })
        .context("no mac field")?;
    if size != image.len() {
        return Err(anyhow!("size mismatch: manifest {size} vs image {}", image.len()));
    }
    if mac.as_slice() != ota_mac(key, stamp, image) {
        return Err(anyhow!("MAC mismatch"));
    }
    Ok(stamp)
}
