// HCA header. Chunk magics have the high bit of each byte set, so signatures are masked with `0x7f`.

use crate::error::{Error, Format, Result, ensure};

use super::SAMPLES_PER_BLOCK;

// The `loop` chunk, converted from blocks to samples
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopInfo {
	pub start_block: u32,
	pub end_block: u32,    // inclusive
	pub start_offset: u16, // samples into `start_block`
	pub end_offset: u16,   // samples dropped from the tail of `end_block`
}

impl LoopInfo {
	// In the trimmed stream's samples
	pub fn start_sample(&self, h: &HcaHeader) -> i64 {
		self.start_block as i64 * SAMPLES_PER_BLOCK as i64 - h.encoder_delay as i64
			+ self.start_offset as i64
	}
	// Exclusive
	pub fn end_sample(&self, h: &HcaHeader) -> i64 {
		(self.end_block as i64 + 1) * SAMPLES_PER_BLOCK as i64
			- self.end_offset as i64
			- h.encoder_delay as i64
	}
}

#[derive(Debug, Clone)]
pub struct HcaHeader {
	pub version: u16,
	pub data_offset: u16, // header size in bytes; block data starts here
	pub channels: u8,
	pub sample_rate: u32,
	pub block_count: u32,
	pub encoder_delay: u16,   // priming discarded at the head
	pub encoder_padding: u16, // padding discarded at the tail
	pub block_size: u16,      // in bytes
	pub min_resolution: u8,
	pub max_resolution: u8,
	pub track_count: u8,
	pub channel_config: u8,
	pub total_band_count: u8,
	pub base_band_count: u8,
	pub stereo_band_count: u8,
	pub bands_per_hfr_group: u8,
	pub ms_stereo: u8,
	pub ath_type: u16,
	pub ciph_type: u16, // 0 plaintext, 1 static table, 56 keyed
	pub rva_volume: f32,
	pub loop_info: Option<LoopInfo>,
}

impl HcaHeader {
	// PCM samples after trimming encoder delay and padding: the ACB's `WaveformTable.NumSamples`
	pub fn total_samples(&self) -> i64 {
		self.block_count as i64 * SAMPLES_PER_BLOCK as i64
			- self.encoder_delay as i64
			- self.encoder_padding as i64
	}

	pub fn parse(buf: &[u8]) -> Result<Self> {
		let sig = |p: usize| -> Option<[u8; 4]> {
			let s = buf.get(p..p + 4)?;
			Some([s[0] & 0x7f, s[1] & 0x7f, s[2] & 0x7f, s[3] & 0x7f])
		};
		let bytes = |p: usize, n: usize| -> Result<&[u8]> {
			buf.get(p..p + n).ok_or(Error::truncated(Format::Hca, p + n, buf.len()))
		};
		let be16 = |p| -> Result<u16> { Ok(u16::from_be_bytes(bytes(p, 2)?.try_into().unwrap())) };
		let be32 = |p| -> Result<u32> { Ok(u32::from_be_bytes(bytes(p, 4)?.try_into().unwrap())) };

		ensure!(sig(0) == Some(*b"HCA\0"), Error::malformed(Format::Hca, "bad magic"));
		let version = be16(4)?;
		let data_offset = be16(6)?;
		ensure!(
			data_offset as usize <= buf.len(),
			Error::truncated(Format::Hca, data_offset as usize, buf.len())
		);

		let mut h = Self {
			version,
			data_offset,
			channels: 0,
			sample_rate: 0,
			block_count: 0,
			encoder_delay: 0,
			encoder_padding: 0,
			block_size: 0,
			min_resolution: 0,
			max_resolution: 0,
			track_count: 0,
			channel_config: 0,
			total_band_count: 0,
			base_band_count: 0,
			stereo_band_count: 0,
			bands_per_hfr_group: 0,
			ms_stereo: 0,
			ath_type: 0,
			ciph_type: 0,
			rva_volume: 1.0,
			loop_info: None,
		};

		let mut seen_fmt = false;
		let mut seen_comp = false;
		let mut p = 8usize;
		// The last two bytes are the header's CRC.
		let end = (data_offset as usize).saturating_sub(2);

		while p < end {
			let Some(s) = sig(p) else { break };
			match &s {
				b"fmt\0" => {
					let ci = be32(p + 4)?;
					h.channels = (ci >> 24) as u8;
					h.sample_rate = ci & 0x00ff_ffff;
					h.block_count = be32(p + 8)?;
					h.encoder_delay = be16(p + 12)?;
					h.encoder_padding = be16(p + 14)?;
					seen_fmt = true;
					p += 16;
				}
				b"comp" => {
					h.block_size = be16(p + 4)?;
					let b = buf.get(p + 6..p + 16).ok_or(Error::truncated(Format::Hca, p + 16, buf.len()))?;
					h.min_resolution = b[0];
					h.max_resolution = b[1];
					h.track_count = b[2];
					h.channel_config = b[3];
					h.total_band_count = b[4];
					h.base_band_count = b[5];
					h.stereo_band_count = b[6];
					h.bands_per_hfr_group = b[7];
					h.ms_stereo = b[8];
					seen_comp = true;
					p += 16;
				}
				b"dec\0" => {
					h.block_size = be16(p + 4)?;
					let b = buf.get(p + 6..p + 12).ok_or(Error::truncated(Format::Hca, p + 12, buf.len()))?;
					h.min_resolution = b[0];
					h.max_resolution = b[1];
					h.total_band_count = b[2] + 1;
					h.base_band_count = b[3] + 1;
					h.track_count = b[4] >> 4;
					h.channel_config = b[4] & 0x0f;
					// `dec` stores a stereo-type flag rather than a stereo band count.
					h.stereo_band_count = if b[5] != 0 { h.total_band_count - h.base_band_count } else { 0 };
					seen_comp = true;
					p += 12;
				}
				b"vbr\0" => p += 8,
				b"ath\0" => {
					h.ath_type = be16(p + 4)?;
					p += 6;
				}
				b"loop" => {
					h.loop_info = Some(LoopInfo {
						start_block: be32(p + 4)?,
						end_block: be32(p + 8)?,
						start_offset: be16(p + 12)?,
						end_offset: be16(p + 14)?,
					});
					p += 16;
				}
				b"ciph" => {
					h.ciph_type = be16(p + 4)?;
					p += 6;
				}
				b"rva\0" => {
					h.rva_volume = f32::from_bits(be32(p + 4)?);
					p += 8;
				}
				b"comm" => {
					let n = *buf.get(p + 4).ok_or(Error::truncated(Format::Hca, p + 5, buf.len()))? as usize;
					p += 5 + n;
				}
				b"pad\0" => break,
				other => {
					let name = String::from_utf8_lossy(other);
					return Err(Error::unsupported(Format::Hca, format!("chunk {name:?} at {p:#x}")));
				}
			}
		}

		ensure!(seen_fmt, Error::malformed(Format::Hca, "no fmt chunk"));
		ensure!(seen_comp, Error::malformed(Format::Hca, "no comp or dec chunk"));
		let channels = || Error::unsupported(Format::Hca, format!("channel count {}", h.channels));
		ensure!((1..=16).contains(&h.channels), channels());
		ensure!(h.block_size != 0, Error::malformed(Format::Hca, "zero block size"));
		Ok(h)
	}
}
