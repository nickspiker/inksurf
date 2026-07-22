//! tide-core — no_std harmonic tide prediction (and, later, rendering) shared by
//! the Raspberry-Pi daemon and the nRF52840 firmware.
//!
//! The prediction path takes an absolute UTC instant (Unix seconds) and baked
//! NOAA harmonic constituents, and returns tide height in feet on the station's
//! MSL datum. No network, no allocation, no std — all float math via `libm`.

#![cfg_attr(not(test), no_std)]

pub mod astro;
pub mod harmonic;
pub mod stations;

pub use harmonic::{predict, terms, Term};
pub use stations::BREMERTON;
