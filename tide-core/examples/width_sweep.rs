//! Spirix width sweep through the *real* harmonic core.
//!
//! The earlier proof (`spirix/examples/tide_predict.rs`) fed raw ωt (billions of radians) into cos() to stress argument reduction. The real model computes the equilibrium argument V from mean longitudes already mod-360, so every cos() argument is a small angle — the reduction stress is gone. What remains is the 37-term accumulation `Σ Hᵢ·fᵢ·cos(Vᵢ+uᵢ−κᵢ)`.
//!
//! This sums the real per-constituent terms (astronomy done in f64 by tide-core) at several Spirix widths, comparing each against the f64 reference AND against a year of NOAA ground truth. Confirms F5E3 (32-bit fraction) is sufficient.
//!
//! Run: cargo run --release --example width_sweep --target x86_64-unknown-linux-gnu

use spirix::*;
use tide_core::{terms, Term, BREMERTON};

const D2R: f64 = std::f64::consts::PI / 180.0;

/// Sum the real terms at a chosen Spirix width. cos() runs in the Scalar type, so this is exactly the precision-sensitive accumulation a device would do.
macro_rules! sum_at {
    ($S:ty, $terms:expr) => {{
        type S = $S;
        let d2r = S::from(D2R);
        let mut h = S::from(0.0_f64);
        for t in $terms {
            let arg = S::from(t.phase_deg) * d2r;
            h = h + S::from(t.amplitude) * arg.cos();
        }
        h.to_f64()
    }};
}

fn main() {
    let raw = include_str!("../tests/noaa_9445958_2026.csv");
    let truth: Vec<(f64, f64)> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (t, v) = l.split_once(',').unwrap();
            (t.parse().unwrap(), v.parse().unwrap())
        })
        .collect();

    println!("Spirix width sweep through the real harmonic core (Bremerton, {} hourly samples)\n", truth.len());
    println!("  {:34} {:>12} {:>12} {:>10}", "width", "max vs f64", "RMS vs NOAA", "verdict");

    let widths: &[(&str, fn(&[Term]) -> f64)] = &[
        ("F4E3 (16b frac)",        |t| sum_at!(ScalarF4E3, t)),
        ("F5E3 (32b frac, ~f32)",  |t| sum_at!(ScalarF5E3, t)),
        ("F6E4 (64b frac, ~f64)",  |t| sum_at!(ScalarF6E4, t)),
    ];

    let mut buf = [Term::default(); 64];
    for (label, f) in widths {
        let mut max_vs_f64 = 0.0_f64;
        let mut ss_noaa = 0.0_f64;
        for &(t, noaa) in &truth {
            let n = terms(BREMERTON, t, &mut buf);
            let terms_slice = &buf[..n];
            // f64 reference sum.
            let mut ref_h = 0.0;
            for term in terms_slice {
                ref_h += term.amplitude * (term.phase_deg * D2R).cos();
            }
            let got = f(terms_slice);
            max_vs_f64 = max_vs_f64.max((got - ref_h).abs());
            let e = got - noaa;
            ss_noaa += e * e;
        }
        let rms_noaa = (ss_noaa / truth.len() as f64).sqrt();
        // Clock-margin floor is ~0.0005 ft; NOAA reconstruction floor is ~0.07 ft.
        let verdict = if max_vs_f64 < 0.0005 { "PASS" } else if max_vs_f64 < 0.05 { "marginal" } else { "BROKEN" };
        println!("  {label:34} {max_vs_f64:12.6} {rms_noaa:12.4} {verdict:>10}");
    }
    println!("\n(max vs f64 = pure numeric error of the width; RMS vs NOAA = total model error,");
    println!(" floored at ~0.07 ft by the 37-constituent reconstruction regardless of width.)");
}
