// ADX ADPCM in `f32`, as CRI's own decoder runs it: normalized `f32` out and `f32` predictor
// history, unlike the usual integer ADX decoders

use crate::error::{Error, Format, Result, ensure};

pub const SAMPLES_PER_BLOCK: usize = 32;
pub const BLOCK_SIZE: usize = 18;

// Added to both history samples after each block
const HISTORY_BIAS: f32 = f32::from_bits(0x3600_0100);
const SAMPLE_SCALE: f32 = 1.0 / 32768.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdxLoop {
	pub start_sample: u32,
	pub end_sample: u32,
	pub start_byte: u32,
	pub end_byte: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdxHeader {
	pub data_offset: usize,
	pub encoding_type: u8,
	pub block_size: u8,
	pub bits_per_sample: u8,
	pub channels: u8,
	pub sample_rate: u32,
	pub sample_count: u32,
	pub highpass_frequency: u16,
	pub version: u8,
	pub revision: u8,
	// `(newest, previous)` per channel, stored as signed 16-bit PCM and normalized before use
	pub initial_history: Vec<[i16; 2]>,
	pub loop_info: Option<AdxLoop>,
}

impl AdxHeader {
	pub fn parse(buf: &[u8]) -> Result<Self> {
		ensure!(buf.len() >= 24, Error::truncated(Format::Adx, 24, buf.len()));
		ensure!(be16(buf, 0)? == 0x8000, Error::malformed(Format::Adx, "bad magic"));

		let data_offset = be16(buf, 2)? as usize + 4;
		ensure!(
			data_offset >= 6,
			Error::malformed(Format::Adx, format!("data offset {data_offset} is too small"))
		);
		ensure!(data_offset <= buf.len(), Error::truncated(Format::Adx, data_offset, buf.len()));
		ensure!(
			&buf[data_offset - 6..data_offset] == b"(c)CRI",
			Error::malformed(Format::Adx, "no copyright marker")
		);

		let encoding_type = buf[4];
		let block_size = buf[5];
		let bits_per_sample = buf[6];
		let channels = buf[7];
		let sample_rate = be32(buf, 8)?;
		let sample_count = be32(buf, 12)?;
		let highpass_frequency = be16(buf, 16)?;
		let version = buf[18];
		let revision = buf[19];

		ensure!(
			encoding_type == 3,
			Error::unsupported(Format::Adx, format!("encoding type {encoding_type}"))
		);
		ensure!(
			block_size as usize == BLOCK_SIZE,
			Error::unsupported(Format::Adx, format!("block size {block_size}"))
		);
		ensure!(
			bits_per_sample == 4,
			Error::unsupported(Format::Adx, format!("sample width {bits_per_sample}"))
		);
		ensure!(
			(1..=8).contains(&channels),
			Error::unsupported(Format::Adx, format!("channel count {channels}"))
		);
		ensure!(sample_rate != 0, Error::malformed(Format::Adx, "zero sample rate"));
		ensure!(version == 4, Error::unsupported(Format::Adx, format!("version {version}")));
		ensure!(
			matches!(revision, 0 | 9),
			Error::unsupported(Format::Adx, format!("v4 revision {revision}"))
		);

		let history_offset = 0x18;
		let history_size = if channels == 1 { 8 } else { channels as usize * 4 };
		let history_end = history_offset + history_size;
		ensure!(
			history_end <= data_offset - 6,
			Error::malformed(Format::Adx, "the v4 history overlaps the copyright marker")
		);
		let mut initial_history = Vec::with_capacity(channels as usize);
		for channel in 0..channels as usize {
			let p = history_offset + channel * 4;
			initial_history.push([be_i16(buf, p)?, be_i16(buf, p + 2)?]);
		}

		// A v4 loop header follows the history when its 24 bytes fit. `AINF` can sit here in a
		// non-looping file, with its tag where the loop flag would be.
		let loop_offset = history_end;
		let header_payload_end = data_offset - 6;
		let ainf = buf.get(loop_offset + 4..loop_offset + 8) == Some(b"AINF".as_slice());
		let loop_info =
			if !ainf && loop_offset + 24 <= header_payload_end && be32(buf, loop_offset + 4)? != 0 {
				let info = AdxLoop {
					start_sample: be32(buf, loop_offset + 8)?,
					start_byte: be32(buf, loop_offset + 12)?,
					end_sample: be32(buf, loop_offset + 16)?,
					end_byte: be32(buf, loop_offset + 20)?,
				};
				ensure!(
					info.start_sample <= info.end_sample,
					Error::malformed(Format::Adx, "the loop starts after it ends")
				);
				ensure!(
					info.end_sample <= sample_count,
					Error::malformed(Format::Adx, "the loop ends past the sample count")
				);
				Some(info)
			} else {
				None
			};

		Ok(Self {
			data_offset,
			encoding_type,
			block_size,
			bits_per_sample,
			channels,
			sample_rate,
			sample_count,
			highpass_frequency,
			version,
			revision,
			initial_history,
			loop_info,
		})
	}

	pub fn block_count(&self) -> usize {
		(self.sample_count as usize).div_ceil(SAMPLES_PER_BLOCK)
	}

	pub fn encoded_data_len(&self) -> Result<usize> {
		self
			.block_count()
			.checked_mul(self.channels as usize)
			.and_then(|n| n.checked_mul(BLOCK_SIZE))
			.ok_or_else(|| Error::malformed(Format::Adx, "the encoded length overflows usize"))
	}
}

#[derive(Debug)]
struct Decoder {
	channels: usize,
	coefficients: [[f32; 4]; 6],
	history: Vec<[f32; 2]>,
	cipher: Option<CipherState>,
}

impl Decoder {
	fn new(header: &AdxHeader, keycode: Option<u64>, subkey: u16) -> Result<Self> {
		let coefficients = coefficient_vectors(header.highpass_frequency, header.sample_rate);
		let history = (header.initial_history.iter())
			.map(|h| [h[0] as f32 / 32768.0, h[1] as f32 / 32768.0])
			.collect();
		// The header parser allows revisions 0 and 9 only.
		let cipher = if header.revision == 9 {
			let keycode = keycode.ok_or(Error::MissingKeycode { format: Format::Adx })?;
			Some(CipherState::derive(keycode, subkey)?)
		} else {
			None
		};
		Ok(Self { channels: header.channels as usize, coefficients, history, cipher })
	}

	// One 18-byte block per channel into 32 interleaved frames
	fn decode_block(&mut self, blocks: &[u8], out: &mut Vec<f32>) -> Result<()> {
		let needed = self.channels * BLOCK_SIZE;
		ensure!(blocks.len() >= needed, Error::truncated(Format::Adx, needed, blocks.len()));

		let base = out.len();
		out.resize(base + self.channels * SAMPLES_PER_BLOCK, 0.0);
		for channel in 0..self.channels {
			let p = channel * BLOCK_SIZE;
			let block = &blocks[p..p + BLOCK_SIZE];
			let raw_scale = u16::from_be_bytes(block[..2].try_into().unwrap());
			if raw_scale & 0x8000 != 0 {
				out.truncate(base);
				return Err(Error::malformed(
					Format::Adx,
					format!("an early end block for channel {channel}"),
				));
			}
			let key = self.cipher.as_ref().map_or(0, |cipher| cipher.xor);
			let scale = (((raw_scale ^ key) & 0x1fff) as f32 + 1.0) * SAMPLE_SCALE;
			if let Some(cipher) = &mut self.cipher {
				cipher.advance();
			}
			let mut residual = [0.0f32; SAMPLES_PER_BLOCK];
			for (i, byte) in block[2..].iter().copied().enumerate() {
				residual[i * 2] = signed_nibble(byte >> 4);
				residual[i * 2 + 1] = signed_nibble(byte & 0x0f);
			}

			let scaled: [[f32; 4]; 4] =
				std::array::from_fn(|source| self.coefficients[source + 2].map(|c| scale * c));
			let [mut newest, mut previous] = self.history[channel];
			for group in 0..8 {
				let r = &residual[group * 4..group * 4 + 4];
				let mut decoded = [0.0f32; 4];
				for lane in 0..4 {
					// The two predictor products first, then the residuals in order: `f32` rounding
					// depends on the grouping.
					let predictor =
						previous * self.coefficients[1][lane] + newest * self.coefficients[0][lane];
					let s0 = predictor + r[0] * scaled[0][lane];
					let s1 = s0 + r[1] * scaled[1][lane];
					let s2 = s1 + r[2] * scaled[2][lane];
					decoded[lane] = s2 + r[3] * scaled[3][lane];
				}

				for (lane, value) in decoded.iter().copied().enumerate() {
					let frame = group * 4 + lane;
					out[base + frame * self.channels + channel] = value;
				}
				newest = decoded[3];
				previous = decoded[2];
			}
			self.history[channel] = [newest + HISTORY_BIAS, previous + HISTORY_BIAS];
		}
		Ok(())
	}
}

// A whole stream, the padded last block trimmed to the header's sample count
pub fn decode_stream(
	adx: &[u8],
	keycode: Option<u64>,
	subkey: u16,
) -> Result<(AdxHeader, Vec<f32>)> {
	let header = AdxHeader::parse(adx)?;
	let encoded_len = header.encoded_data_len()?;
	let end = header
		.data_offset
		.checked_add(encoded_len)
		.ok_or_else(|| Error::malformed(Format::Adx, "the stream length overflows usize"))?;
	ensure!(adx.len() >= end, Error::truncated(Format::Adx, end, adx.len()));

	let mut decoder = Decoder::new(&header, keycode, subkey)?;
	let group_size = header.channels as usize * BLOCK_SIZE;
	let mut samples =
		Vec::with_capacity(header.block_count() * SAMPLES_PER_BLOCK * header.channels as usize);
	for group in adx[header.data_offset..end].chunks_exact(group_size) {
		decoder.decode_block(group, &mut samples)?;
	}
	samples.truncate(header.sample_count as usize * header.channels as usize);
	Ok((header, samples))
}

// Type 9 encrypts each channel block's scale word, one LCG step per channel block.
#[derive(Debug, Clone, Copy)]
pub struct CipherState {
	pub xor: u16,
	pub multiplier: u16,
	pub increment: u16,
}

impl CipherState {
	pub fn derive(mut keycode: u64, subkey: u16) -> Result<Self> {
		ensure!(keycode != 0, Error::MissingKeycode { format: Format::Adx });
		if subkey != 0 {
			let low = (!subkey).wrapping_add(2) as u64;
			keycode = keycode.wrapping_mul(((subkey as u64) << 16) | low);
		}
		keycode = keycode.wrapping_sub(1);
		Ok(Self {
			xor: ((keycode >> 27) & 0x7fff) as u16,
			multiplier: (((keycode >> 12) & 0x7ffc) | 1) as u16,
			increment: (((keycode << 1) & 0x7fff) | 1) as u16,
		})
	}

	fn advance(&mut self) {
		self.xor = self.xor.wrapping_mul(self.multiplier).wrapping_add(self.increment);
	}
}

fn coefficient_vectors(cutoff: u16, sample_rate: u32) -> [[f32; 4]; 6] {
	// In `f32`, truncating where the reference does, so any sample rate gives its coefficients.
	let sqrt2 = 2.0f32.sqrt();
	let a = sqrt2 - ((cutoff as f32 * f32::from_bits(0x40c9_0fdb)) / sample_rate as f32).cos();
	let b = sqrt2 - 1.0;
	let c = (a - ((a - b) * (b + a)).sqrt()) / b;
	let c1 = (((c + c) * 4096.0) as i16) as f32 * (1.0 / 4096.0);
	let c2 = (((c * c) * -4096.0) as i16) as f32 * (1.0 / 4096.0);

	// The predictor's response to the two history values and then each of four residuals, laid out
	// four lanes wide.
	let mut flat = [0.0f32; 24];
	flat[8] = 1.0;
	flat[13] = 1.0;
	flat[0] = c1;
	flat[4] = c2;
	flat[9] = c1;
	flat[1] = c1 * c1 + c2;
	flat[5] = c2 * c1;
	for start in 0..2 {
		let mut i = start;
		for _ in 0..6 {
			flat[i + 2] = c1 * flat[i + 1] + c2 * flat[i];
			i += 4;
		}
		flat[18 + start * 5] = 1.0;
	}
	let mut out = [[0.0f32; 4]; 6];
	for (dst, src) in out.iter_mut().zip(flat.as_chunks::<4>().0) {
		*dst = *src;
	}
	out
}

fn signed_nibble(n: u8) -> f32 {
	if n < 8 { n as f32 } else { n as f32 - 16.0 }
}

fn be16(buf: &[u8], p: usize) -> Result<u16> {
	let bytes = buf.get(p..p + 2).ok_or(Error::truncated(Format::Adx, p + 2, buf.len()))?;
	Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
}

fn be_i16(buf: &[u8], p: usize) -> Result<i16> {
	Ok(be16(buf, p)? as i16)
}

fn be32(buf: &[u8], p: usize) -> Result<u32> {
	let bytes = buf.get(p..p + 4).ok_or(Error::truncated(Format::Adx, p + 4, buf.len()))?;
	Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn fixture(channels: u8, sample_count: u32, loop_info: Option<AdxLoop>) -> Vec<u8> {
		let history_size = if channels == 1 { 8 } else { channels as usize * 4 };
		let payload_end = 0x18 + history_size + if loop_info.is_some() { 24 } else { 0 };
		let data_offset = payload_end + 6;
		let mut out = vec![0u8; data_offset];
		out[0..2].copy_from_slice(&0x8000u16.to_be_bytes());
		out[2..4].copy_from_slice(&((data_offset - 4) as u16).to_be_bytes());
		out[4] = 3;
		out[5] = BLOCK_SIZE as u8;
		out[6] = 4;
		out[7] = channels;
		out[8..12].copy_from_slice(&48000u32.to_be_bytes());
		out[12..16].copy_from_slice(&sample_count.to_be_bytes());
		out[16..18].copy_from_slice(&500u16.to_be_bytes());
		out[18] = 4;
		if let Some(l) = loop_info {
			let p = 0x18 + history_size;
			out[p + 4..p + 8].copy_from_slice(&1u32.to_be_bytes());
			out[p + 8..p + 12].copy_from_slice(&l.start_sample.to_be_bytes());
			out[p + 12..p + 16].copy_from_slice(&l.start_byte.to_be_bytes());
			out[p + 16..p + 20].copy_from_slice(&l.end_sample.to_be_bytes());
			out[p + 20..p + 24].copy_from_slice(&l.end_byte.to_be_bytes());
		}
		out[payload_end..data_offset].copy_from_slice(b"(c)CRI");
		out.resize(
			data_offset
				+ (sample_count as usize).div_ceil(SAMPLES_PER_BLOCK) * channels as usize * BLOCK_SIZE,
			0,
		);
		out
	}

	#[test]
	fn v4_header() {
		let lp = AdxLoop { start_sample: 12, end_sample: 63, start_byte: 100, end_byte: 136 };
		let adx = fixture(2, 63, Some(lp));
		let h = AdxHeader::parse(&adx).unwrap();
		assert_eq!(h.channels, 2);
		assert_eq!(h.sample_rate, 48000);
		assert_eq!(h.block_count(), 2);
		assert_eq!(h.initial_history, vec![[0, 0], [0, 0]]);
		assert_eq!(h.loop_info, Some(lp));
	}

	#[test]
	fn interleaved_and_trimmed() {
		let mut adx = fixture(2, 3, None);
		let data = AdxHeader::parse(&adx).unwrap().data_offset;
		// Left residual +1 at scale 1/32768; right residual -1 at scale 2/32768.
		adx[data + 2] = 0x10;
		adx[data + BLOCK_SIZE..data + BLOCK_SIZE + 2].copy_from_slice(&1u16.to_be_bytes());
		adx[data + BLOCK_SIZE + 2] = 0xf0;
		let (h, pcm) = decode_stream(&adx, None, 0).unwrap();
		assert_eq!(pcm.len(), h.sample_count as usize * h.channels as usize);
		assert_eq!(pcm[0].to_bits(), (1.0f32 / 32768.0).to_bits());
		assert_eq!(pcm[1].to_bits(), (-2.0f32 / 32768.0).to_bits());
		assert!(pcm[2].is_finite());
		assert!(pcm[3].is_finite());
	}

	#[test]
	fn type9_key() {
		let mut adx = fixture(2, 32, None);
		adx[19] = 9;
		let h = AdxHeader::parse(&adx).unwrap();
		assert!(Decoder::new(&h, None, 0).unwrap_err().to_string().contains("keycode"));
		let key = CipherState::derive(crate::TEST_KEYCODE, 0).unwrap();
		assert_eq!((key.xor, key.multiplier, key.increment), (11505, 6845, 7133));
	}
}
