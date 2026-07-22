//! Astronomical arguments for tidal harmonic prediction.
//!
//! Port of pytides' `astro.py` (which follows Meeus for the mean longitudes and
//! Schureman for the nodal auxiliaries). All angles are in degrees. Input is an
//! absolute UTC instant as a Unix timestamp — no timezone or DST enters here.
//!
//! The mean-longitude polynomials are in `T` = Julian centuries since J2000.
//! We evaluate everything at the actual prediction time, so a constituent's
//! equilibrium argument `V(t)` already carries its full time evolution (it is
//! linear in these mean longitudes, which advance at the constituent's speed).

use libm::{atan, cos, floor, sin, tan};

const D2R: f64 = core::f64::consts::PI / 180.0;
const R2D: f64 = 180.0 / core::f64::consts::PI;

/// Julian Date from a Unix timestamp (seconds since 1970-01-01 00:00 UTC).
#[inline]
fn julian_date(unix_secs: f64) -> f64 {
    unix_secs / 86_400.0 + 2_440_587.5
}

/// Evaluate a polynomial `c[0] + c[1]x + c[2]x^2 + …` at `x`.
#[inline]
fn poly(coeffs: &[f64], x: f64) -> f64 {
    let mut acc = 0.0;
    let mut xp = 1.0;
    for &c in coeffs {
        acc += c * xp;
        xp *= x;
    }
    acc
}

#[inline]
fn norm360(deg: f64) -> f64 {
    let m = deg % 360.0;
    if m < 0.0 {
        m + 360.0
    } else {
        m
    }
}

// Meeus mean-longitude polynomials (degrees, argument T = Julian centuries J2000).
const LUNAR_LONGITUDE: [f64; 5] = [218.316_459_1, 481_267.881_342_36, -0.001_326_8, 1.0 / 538_841.0, -1.0 / 65_194_000.0];
const SOLAR_LONGITUDE: [f64; 3] = [280.466_45, 36_000.769_83, 0.000_303_2];
const LUNAR_PERIGEE: [f64; 5] = [83.353_243_0, 4_069.013_711_1, -0.010_323_8, -1.0 / 80_053.0, 1.0 / 18_999_000.0];
const LUNAR_NODE: [f64; 5] = [125.044_555_0, -1_934.136_184_9, 0.002_076_2, 1.0 / 467_410.0, -1.0 / 60_616_000.0];
// Solar perigee: Meeus 24.2 − 24.3 (per pytides).
const SOLAR_PERIGEE: [f64; 4] = [280.466_45 - 357.529_10, 36_000.769_32 - 35_999.050_30, 0.000_303_2 + 0.000_155_9, 0.000_000_48];
// Mean obliquity of the ecliptic, Meeus 22.2 (degrees, in T).
const OBLIQUITY: [f64; 4] = [23.439_291_111, -0.013_004_167, -0.000_000_164, 0.000_000_504];
// Lunar orbital inclination to the ecliptic (degrees), essentially constant.
const LUNAR_INCLINATION: f64 = 5.145;

/// The astronomical arguments needed to assemble equilibrium arguments and node
/// factors, all in degrees, evaluated at one instant.
#[derive(Clone, Copy)]
pub struct Astro {
    pub t_plus_h_minus_s: f64, // T+h−s (hour angle of mean sun + h − s)
    pub s: f64,                // moon mean longitude
    pub h: f64,                // sun mean longitude
    pub p: f64,                // lunar perigee
    pub n: f64,                // lunar ascending node
    pub pp: f64,               // solar perigee
    pub ninety: f64,           // constant 90°
    pub xi: f64,
    pub nu: f64,
    pub nup: f64,
    pub nupp: f64,
    pub big_i: f64, // I
    pub p_schureman: f64, // P = p − xi
    pub omega: f64, // obliquity
    pub little_i: f64, // lunar inclination
}

/// Schureman I: obliquity of the lunar orbit to the equator.
fn calc_i(n: f64, i: f64, omega: f64) -> f64 {
    let (n, i, omega) = (n * D2R, i * D2R, omega * D2R);
    let cos_i = cos(i) * cos(omega) - sin(i) * sin(omega) * cos(n);
    R2D * libm::acos(cos_i)
}

// e1/e2 shared by xi and nu.
fn e1_e2(n: f64, i: f64, omega: f64) -> (f64, f64) {
    let (n, i, omega) = (n * D2R, i * D2R, omega * D2R);
    let e1 = atan(cos(0.5 * (omega - i)) / cos(0.5 * (omega + i)) * tan(0.5 * n));
    let e2 = atan(sin(0.5 * (omega - i)) / sin(0.5 * (omega + i)) * tan(0.5 * n));
    (e1 - 0.5 * n, e2 - 0.5 * n)
}

fn calc_xi(n: f64, i: f64, omega: f64) -> f64 {
    let (e1, e2) = e1_e2(n, i, omega);
    -(e1 + e2) * R2D
}

fn calc_nu(n: f64, i: f64, omega: f64) -> f64 {
    let (e1, e2) = e1_e2(n, i, omega);
    (e1 - e2) * R2D
}

/// Schureman 224.
fn calc_nup(big_i_deg: f64, nu_deg: f64) -> f64 {
    let (big_i, nu) = (big_i_deg * D2R, nu_deg * D2R);
    R2D * atan(sin(2.0 * big_i) * sin(nu) / (sin(2.0 * big_i) * cos(nu) + 0.3347))
}

/// Schureman 232.
fn calc_nupp(big_i_deg: f64, nu_deg: f64) -> f64 {
    let (big_i, nu) = (big_i_deg * D2R, nu_deg * D2R);
    let s = sin(big_i);
    let tan2nupp = (s * s * sin(2.0 * nu)) / (s * s * cos(2.0 * nu) + 0.0727);
    R2D * 0.5 * atan(tan2nupp)
}

/// Compute all astronomical arguments at a Unix timestamp.
pub fn astro(unix_secs: f64) -> Astro {
    let jd = julian_date(unix_secs);
    let t = (jd - 2_451_545.0) / 36_525.0;

    let s = norm360(poly(&LUNAR_LONGITUDE, t));
    let h = norm360(poly(&SOLAR_LONGITUDE, t));
    let p = norm360(poly(&LUNAR_PERIGEE, t));
    let n = norm360(poly(&LUNAR_NODE, t));
    let pp = norm360(poly(&SOLAR_PERIGEE, t));
    let omega = norm360(poly(&OBLIQUITY, t));
    let i = LUNAR_INCLINATION;

    let big_i = calc_i(n, i, omega);
    let xi = calc_xi(n, i, omega);
    let nu = calc_nu(n, i, omega);
    let nup = calc_nup(big_i, nu);
    let nupp = calc_nupp(big_i, nu);
    let p_schureman = norm360(p - xi);

    // Hour angle of mean sun: frac(JD)*360 (JD is .5 at 00:00 UTC → 180°).
    let hour = (jd - floor(jd)) * 360.0;
    let t_plus_h_minus_s = hour + h - s;

    Astro {
        t_plus_h_minus_s,
        s,
        h,
        p,
        n,
        pp,
        ninety: 90.0,
        xi,
        nu,
        nup,
        nupp,
        big_i,
        p_schureman,
        omega,
        little_i: i,
    }
}
