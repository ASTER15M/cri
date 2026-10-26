// HCA, CRI's streaming codec: the container, the cipher and the decoder

mod bitreader;
mod cipher;
mod crc;
mod decoder;
mod header;
mod tables;

pub use cipher::CipherTable;
pub use crc::block_is_intact;
pub use decoder::decode_stream;
pub use header::{HcaHeader, LoopInfo};

pub const SAMPLES_PER_BLOCK: u32 = 1024; // 8 sub-frames; `HcaHeader::block_size` is in bytes
pub const SAMPLES_PER_SUBFRAME: u32 = 128;
