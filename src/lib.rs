// CRI Middleware containers and codecs: ACB/AWB (AFS2) banks and their UTF tables, ADX and HCA
// audio, and the USM movie container. Keycodes are the caller's. The ADX and HCA decoders produce
// the same `f32` samples as CRI's own runtime, bit for bit.

pub mod acb;
pub mod acf;
pub mod adx;
pub mod afs2;
pub mod asr;
mod error;
pub mod hca;
pub mod usm;
pub mod utf;

pub use error::{Error, Format, Result};

// An arbitrary keycode for the cipher tests, with every byte non-zero so each one reaches the
// derivations
#[cfg(test)]
const TEST_KEYCODE: u64 = 0x0123_4567_89ab_cdef;
