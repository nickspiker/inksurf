//! Baked harmonic constituents per station.
//!
//! AUTO-GENERATED from NOAA `harcon.json`. The (H, κ) pairs are station-specific;
//! the Doodson coefficients and node-factor composition are universal.

use crate::harmonic::{C, F, U};

/// Bremerton, WA — NOAA station 9445958. H in feet on MSL datum, κ in degrees GMT.
pub const BREMERTON: &[C] = &[
    C{name:"M2",coeffs:[2,0,0,0,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,1)],h:3.6,kappa:17.0},
    C{name:"S2",coeffs:[2,2,-2,0,0,0,0],f:&[],u:&[],h:0.89,kappa:45.1},
    C{name:"N2",coeffs:[2,-1,0,1,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,1)],h:0.7,kappa:349.0},
    C{name:"K1",coeffs:[1,1,0,0,0,0,-1],f:&[(F::K1,1)],u:&[(U::K1,1)],h:2.73,kappa:280.4},
    C{name:"M4",coeffs:[4,0,0,0,0,0,0],f:&[(F::M2,2)],u:&[(U::M2,2)],h:0.08,kappa:243.9},
    C{name:"O1",coeffs:[1,-1,0,0,0,0,1],f:&[(F::O1,1)],u:&[(U::O1,1)],h:1.5,kappa:257.8},
    C{name:"M6",coeffs:[6,0,0,0,0,0,0],f:&[(F::M2,3)],u:&[(U::M2,3)],h:0.06,kappa:3.0},
    C{name:"MK3",coeffs:[3,1,0,0,0,0,-1],f:&[(F::M2,1),(F::K1,1)],u:&[(U::M2,1),(U::K1,1)],h:0.09,kappa:120.4},
    C{name:"S4",coeffs:[4,4,-4,0,0,0,0],f:&[],u:&[],h:0.01,kappa:289.5},
    C{name:"MN4",coeffs:[4,-1,0,1,0,0,0],f:&[(F::M2,2)],u:&[(U::M2,2)],h:0.04,kappa:213.7},
    C{name:"nu2",coeffs:[2,-1,2,-1,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,1)],h:0.15,kappa:358.1},
    C{name:"S6",coeffs:[6,6,-6,0,0,0,0],f:&[],u:&[],h:0.0,kappa:0.0},
    C{name:"mu2",coeffs:[2,-2,2,0,0,0,0],f:&[(F::M2,2)],u:&[(U::M2,2)],h:0.08,kappa:231.9},
    C{name:"2N2",coeffs:[2,-2,0,2,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,1)],h:0.09,kappa:322.3},
    C{name:"OO1",coeffs:[1,3,0,0,0,0,-1],f:&[(F::OO1,1)],u:&[(U::OO1,1)],h:0.1,kappa:333.4},
    C{name:"lambda2",coeffs:[2,1,-2,1,0,0,2],f:&[(F::M2,1)],u:&[(U::M2,1)],h:0.06,kappa:45.4},
    C{name:"S1",coeffs:[1,1,-1,0,0,0,0],f:&[],u:&[],h:0.09,kappa:17.5},
    C{name:"M1",coeffs:[1,0,0,0,0,0,1],f:&[(F::M1,1)],u:&[(U::M1,1)],h:0.05,kappa:323.0},
    C{name:"J1",coeffs:[1,2,0,-1,0,0,-1],f:&[(F::J1,1)],u:&[(U::J1,1)],h:0.15,kappa:319.5},
    C{name:"Mm",coeffs:[0,1,0,-1,0,0,0],f:&[(F::Mm,1)],u:&[],h:0.0,kappa:0.0},
    C{name:"Ssa",coeffs:[0,0,2,0,0,0,0],f:&[],u:&[],h:0.11,kappa:231.1},
    C{name:"Sa",coeffs:[0,0,1,0,0,0,0],f:&[],u:&[],h:0.25,kappa:292.9},
    C{name:"MSF",coeffs:[0,2,-2,0,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,-1)],h:0.0,kappa:0.0},
    C{name:"Mf",coeffs:[0,2,0,0,0,0,0],f:&[(F::Mf,1)],u:&[(U::Mf,1)],h:0.07,kappa:140.5},
    C{name:"rho1",coeffs:[1,-2,2,-1,0,0,1],f:&[(F::M2,1),(F::K1,1)],u:&[(U::M2,1),(U::K1,-1)],h:0.05,kappa:272.1},
    C{name:"Q1",coeffs:[1,-2,0,1,0,0,1],f:&[(F::O1,1)],u:&[(U::O1,1)],h:0.24,kappa:251.2},
    C{name:"T2",coeffs:[2,2,-3,0,0,1,0],f:&[],u:&[],h:0.06,kappa:49.1},
    C{name:"R2",coeffs:[2,2,-1,0,0,-1,2],f:&[],u:&[],h:0.01,kappa:354.8},
    C{name:"2Q1",coeffs:[1,-3,0,2,0,0,1],f:&[(F::M2,1),(F::J1,1)],u:&[(U::M2,1),(U::J1,-1)],h:0.04,kappa:230.5},
    C{name:"P1",coeffs:[1,1,-2,0,0,0,1],f:&[],u:&[],h:0.83,kappa:281.3},
    C{name:"2SM2",coeffs:[2,4,-4,0,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,-1)],h:0.03,kappa:270.2},
    C{name:"M3",coeffs:[3,0,0,0,0,0,0],f:&[(F::Modd3,1)],u:&[(U::Modd3,1)],h:0.02,kappa:207.2},
    C{name:"L2",coeffs:[2,1,0,-1,0,0,2],f:&[(F::L2,1)],u:&[(U::L2,1)],h:0.12,kappa:63.4},
    C{name:"2MK3",coeffs:[3,-1,0,0,0,0,1],f:&[(F::M2,1),(F::O1,1)],u:&[(U::M2,1),(U::O1,1)],h:0.06,kappa:80.3},
    C{name:"K2",coeffs:[2,2,0,0,0,0,0],f:&[(F::K2,1)],u:&[(U::K2,1)],h:0.26,kappa:39.0},
    C{name:"M8",coeffs:[8,0,0,0,0,0,0],f:&[(F::M2,4)],u:&[(U::M2,4)],h:0.01,kappa:224.8},
    C{name:"MS4",coeffs:[4,2,-2,0,0,0,0],f:&[(F::M2,1)],u:&[(U::M2,1)],h:0.05,kappa:267.2},
];
