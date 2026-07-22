//! Validate the harmonic synthesizer against a full year of NOAA published
//! predictions for Bremerton (station 9445958, MSL datum, hourly, GMT).
//!
//! Fixture: `tests/noaa_9445958_2026.csv` — `unix_secs,height_ft` per line.

use tide_core::{predict, BREMERTON};

fn load_fixture() -> Vec<(f64, f64)> {
    let raw = include_str!("noaa_9445958_2026.csv");
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (t, v) = l.split_once(',').expect("csv row");
            (t.parse().unwrap(), v.parse().unwrap())
        })
        .collect()
}

#[test]
fn matches_noaa_year_within_margin() {
    let truth = load_fixture();
    assert_eq!(truth.len(), 8760, "expected a full year of hourly samples");

    let mut sum_sq = 0.0_f64;
    let mut max_err = 0.0_f64;
    let mut worst_t = 0.0;
    for &(t, noaa) in &truth {
        let got = predict(BREMERTON, t);
        let err = (got - noaa).abs();
        sum_sq += err * err;
        if err > max_err {
            max_err = err;
            worst_t = t;
        }
    }
    let rms = (sum_sq / truth.len() as f64).sqrt();
    println!("NOAA cross-check: RMS={rms:.4} ft  max={max_err:.4} ft  (worst t={worst_t})");

    // Measured: RMS ≈ 0.070 ft, max ≈ 0.25 ft, mean bias ≈ 0.0001 ft (no DC
    // offset, no seasonal structure). A global time-shift scan bottoms out
    // exactly at dt=0, so the timebase/phase convention is correct. The ~0.07 ft
    // residual is the intrinsic limit of reconstructing NOAA's operational
    // predictions from only its 37 *published* constituents — NOAA infers
    // additional minor constituents internally. That's ~2 cm, well under the
    // display's ~0.1 ft/pixel resolution. Bounds below leave headroom for the
    // node factor's year-to-year variation over the 18.6-yr nodal cycle.
    assert!(rms < 0.10, "RMS {rms:.4} ft exceeds the 0.10 ft reconstruction bound");
    assert!(max_err < 0.40, "max error {max_err:.4} ft unexpectedly large");
}
