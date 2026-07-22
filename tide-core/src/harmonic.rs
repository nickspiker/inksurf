//! Harmonic tide synthesis from NOAA constituents + Schureman node factors.
//!
//! `height(t) = Z0 + Σ Hᵢ·fᵢ(t)·cos(Vᵢ(t) + uᵢ(t) − κᵢ)`, with the equilibrium
//! argument `V` assembled from Doodson coefficients dotted with the astronomical
//! arguments (see [`astro`]), the node factor `f` and correction `u` per
//! Schureman, and `H`/`κ` the NOAA published amplitude/phase for the station.
//!
//! Z0 = 0 because the constituents are stated on the station's MSL datum.

use crate::astro::{astro, Astro};
use libm::{atan, cos, pow, sin, sqrt, tan};

const D2R: f64 = core::f64::consts::PI / 180.0;
const R2D: f64 = 180.0 / core::f64::consts::PI;

/// `f64::powi` is std-only; provide an integer power for no_std via
/// exponentiation-by-squaring. (In `test` builds the inherent std method wins;
/// both compute the same value.)
trait Powi {
    fn powi(self, n: i32) -> Self;
}
impl Powi for f64 {
    #[inline]
    fn powi(self, n: i32) -> f64 {
        let mut result = 1.0;
        let mut base = self;
        let mut exp = n.unsigned_abs();
        while exp > 0 {
            if exp & 1 == 1 {
                result *= base;
            }
            base *= base;
            exp >>= 1;
        }
        if n < 0 {
            1.0 / result
        } else {
            result
        }
    }
}

/// Node-factor kinds (Schureman). Evaluated per instant from [`Astro`].
#[derive(Clone, Copy)]
pub enum F {
    M2,
    O1,
    K1,
    K2,
    L2,
    M1,
    Mm,
    Mf,
    J1,
    OO1,
    Modd3,
}

/// Nodal-correction kinds (Schureman).
#[derive(Clone, Copy)]
pub enum U {
    M2,
    O1,
    K1,
    K2,
    L2,
    M1,
    Mf,
    J1,
    OO1,
    Modd3,
}

/// One harmonic constituent: Doodson coefficients, node-factor/correction
/// composition (products of `F`, sums of `U`), and this station's H (ft) and
/// κ (degrees, GMT).
pub struct C {
    pub name: &'static str,
    pub coeffs: [i8; 7],
    pub f: &'static [(F, i8)],
    pub u: &'static [(U, i8)],
    pub h: f64,
    pub kappa: f64,
}

#[inline]
fn cos4(x_rad: f64) -> f64 {
    let c = cos(x_rad);
    c * c * c * c
}

/// Node factor for a single `F` kind at these astronomical arguments.
fn f_value(kind: F, a: &Astro) -> f64 {
    let omega = a.omega * D2R;
    let i = a.little_i * D2R;
    let big_i = a.big_i * D2R;
    let nu = a.nu * D2R;
    match kind {
        F::M2 => cos4(0.5 * big_i) / (cos4(0.5 * omega) * cos4(0.5 * i)),
        F::O1 => {
            let mean = sin(omega) * cos(0.5 * omega).powi(2) * cos4(0.5 * i);
            (sin(big_i) * cos(0.5 * big_i).powi(2)) / mean
        }
        F::K1 => {
            let mean = 0.5023 * sin(2.0 * omega) * (1.0 - 1.5 * sin(i).powi(2)) + 0.1681;
            sqrt(0.2523 * sin(2.0 * big_i).powi(2) + 0.1689 * sin(2.0 * big_i) * cos(nu) + 0.0283) / mean
        }
        F::K2 => {
            let mean = 0.5023 * sin(omega).powi(2) * (1.0 - 1.5 * sin(i).powi(2)) + 0.0365;
            sqrt(0.2523 * sin(big_i).powi(4) + 0.0367 * sin(big_i).powi(2) * cos(2.0 * nu) + 0.0013) / mean
        }
        F::L2 => {
            let p = a.p_schureman * D2R;
            let r_inv = sqrt(1.0 - 12.0 * tan(0.5 * big_i).powi(2) * cos(2.0 * p) + 36.0 * tan(0.5 * big_i).powi(4));
            f_value(F::M2, a) * r_inv
        }
        F::M1 => {
            let p = a.p_schureman * D2R;
            let ci = cos(0.5 * big_i);
            let q_inv = sqrt(0.25 + 1.5 * cos(big_i) * cos(2.0 * p) / sqrt(ci) + 2.25 * cos(big_i).powi(2) / cos4(0.5 * big_i));
            f_value(F::O1, a) * q_inv
        }
        F::Mm => {
            let mean = (2.0 / 3.0 - sin(omega).powi(2)) * (1.0 - 1.5 * sin(i).powi(2));
            (2.0 / 3.0 - sin(big_i).powi(2)) / mean
        }
        F::Mf => {
            let mean = sin(omega).powi(2) * cos(0.5 * i).powi(4);
            sin(big_i).powi(2) / mean
        }
        F::J1 => sin(2.0 * big_i) / (sin(2.0 * omega) * (1.0 - 1.5 * sin(i).powi(2))),
        F::OO1 => {
            let mean = sin(omega) * sin(0.5 * omega).powi(2) * cos4(0.5 * i);
            sin(big_i) * sin(0.5 * big_i).powi(2) / mean
        }
        F::Modd3 => pow(f_value(F::M2, a), 1.5),
    }
}

/// Nodal correction (degrees) for a single `U` kind.
fn u_value(kind: U, a: &Astro) -> f64 {
    match kind {
        U::M2 => 2.0 * a.xi - 2.0 * a.nu,
        U::O1 => 2.0 * a.xi - a.nu,
        U::K1 => -a.nup,
        U::K2 => -2.0 * a.nupp,
        U::J1 => -a.nu,
        U::OO1 => -2.0 * a.xi - a.nu,
        U::Mf => -2.0 * a.xi,
        U::Modd3 => 1.5 * (2.0 * a.xi - 2.0 * a.nu),
        U::L2 => {
            let big_i = a.big_i * D2R;
            let p = a.p_schureman * D2R;
            let r = R2D * atan(sin(2.0 * p) / (1.0 / 6.0 / tan(0.5 * big_i).powi(2) - cos(2.0 * p)));
            2.0 * a.xi - 2.0 * a.nu - r
        }
        U::M1 => {
            let big_i = a.big_i * D2R;
            let p = a.p_schureman * D2R;
            let q = R2D * atan((5.0 * cos(big_i) - 1.0) / (7.0 * cos(big_i) + 1.0) * tan(p));
            a.xi - a.nu + q
        }
    }
}

/// One harmonic term evaluated at an instant: `(amplitude, phase_deg)` where
/// amplitude = Hᵢ·fᵢ (feet) and phase = Vᵢ+uᵢ−κᵢ (degrees). The height
/// contribution is `amplitude · cos(phase_deg · π/180)`.
#[derive(Clone, Copy, Default)]
pub struct Term {
    pub amplitude: f64,
    pub phase_deg: f64,
}

/// Decompose the prediction at `unix_secs` into per-constituent terms, filling
/// `out` and returning the count written. The astronomy (V, u, f) is done in
/// f64 here; the caller does the final `Σ amplitude·cos(phase)` accumulation —
/// which is where a device can tune numeric precision to the clock margin.
pub fn terms(constituents: &[C], unix_secs: f64, out: &mut [Term]) -> usize {
    let a = astro(unix_secs);
    let av = [a.t_plus_h_minus_s, a.s, a.h, a.p, a.n, a.pp, a.ninety];
    let mut count = 0;
    for c in constituents {
        if count >= out.len() {
            break;
        }
        // Equilibrium argument V = Doodson coefficients · astronomical args.
        let mut v = 0.0;
        for k in 0..7 {
            v += c.coeffs[k] as f64 * av[k];
        }
        // Node factor: product of F terms raised to their powers.
        let mut f = 1.0;
        for &(kind, powr) in c.f {
            f *= pow(f_value(kind, &a), powr as f64);
        }
        // Nodal correction: sum of U terms times their multipliers.
        let mut u = 0.0;
        for &(kind, mult) in c.u {
            u += u_value(kind, &a) * mult as f64;
        }
        out[count] = Term {
            amplitude: c.h * f,
            phase_deg: v + u - c.kappa,
        };
        count += 1;
    }
    count
}

/// Predicted tide height (feet, on the station's MSL datum) at a Unix timestamp.
pub fn predict(constituents: &[C], unix_secs: f64) -> f64 {
    let mut buf = [Term::default(); 64];
    let n = terms(constituents, unix_secs, &mut buf);
    let mut height = 0.0;
    for t in &buf[..n] {
        height += t.amplitude * cos(t.phase_deg * D2R);
    }
    height
}
