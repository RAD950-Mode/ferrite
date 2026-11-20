//! # ferrite-core
//!
//! Everything in Ferrite that is plain logic: what the tasks say to each other ([`msg`]), frequencies ([`freq`]),
//! keys ([`keys`]), the SPI flash map with its protected ranges ([`layout`]), the one decoder slot all decoders
//! share ([`decoders`]), and chip identification ([`chipid`]). No registers, no async runtime: it all builds and
//! tests on the PC.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod acc;
pub mod audio;
pub mod battery;
pub mod chipid;
pub mod clocktree;
pub mod cmdq;
pub mod sha256;
pub mod rad950;
pub mod ssb;
pub mod stuck;
pub mod dcs;
pub mod decoders;
pub mod dtmfrx;
#[cfg(feature = "bench")]
pub mod fskbench;
pub mod freq;
pub mod gps;
pub mod gpspower;
pub mod keys;
pub mod layout;
pub mod lease;
pub mod link;
pub mod linkz;
pub mod linkz_data;
#[cfg(test)]
mod linkz_vectors;
pub mod liveview;
pub mod mcutemp;
#[cfg(feature = "bench")]
pub mod memmap;
pub mod msg;
pub mod name;
pub mod part;
pub mod path;
pub mod rawaudio;
pub mod rxmeter;
#[cfg(feature = "bench")]
pub mod synthfsk;
pub mod reader;
pub mod restore;
pub mod rxaudio;
pub mod rxlevel;
pub mod rxsleep;
pub mod same;
pub mod same_rx;
pub mod sat;
pub mod scan;
pub mod settle;
pub mod softsq;
pub mod aprssq;
pub mod sqcal;
pub mod sweephold;
pub mod tonegen;
pub mod tonehint;
pub mod tonesearch;
pub mod txcal;
pub mod txeq;
pub mod txfilters;
pub mod txplan;
pub mod txseq;
#[cfg(feature = "bench")]
pub mod txsuite;
#[cfg(feature = "bench")]
pub mod txtest;
pub mod uid;
pub mod vox;
pub mod watch;
pub mod xb;

/// Shown on the boot screen and in the link's hello.
pub const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// The build (SELF-5): the git commit, short, "+" when built from uncommitted changes (build.rs).
pub const BUILD: &str = env!("FERRITE_BUILD");

/// Set by the simulators' screenshot tests, so pictures don't change with every commit: [`build`] then reads
/// "0000000".
pub static BUILD_FIXED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The build ID to show (SELF-5).
pub fn build() -> &'static str {
    if BUILD_FIXED.load(core::sync::atomic::Ordering::Relaxed) { "0000000" } else { BUILD }
}

#[cfg(test)]
mod build_tests {
    use core::sync::atomic::Ordering::Relaxed;

    /// SELF-5: the commit, or the fixed ID the screenshot tests set (nothing else in this crate reads it).
    #[test]
    fn build_id_or_fixed() {
        assert_eq!(super::build(), super::BUILD);
        super::BUILD_FIXED.store(true, Relaxed);
        assert_eq!(super::build(), "0000000");
        super::BUILD_FIXED.store(false, Relaxed);
        assert_eq!(super::build(), super::BUILD);
    }
}
