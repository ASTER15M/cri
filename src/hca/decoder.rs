// HCA decoder, stage by stage, matching CRI's `f32` output bit for bit. Operand order is kept on
// subtractions, and nothing may use `f32::mul_add`, which rounds once where the reference rounds
// twice.

use crate::error::{Error, Format, Result, ensure};

use super::bitreader::BitReader;
use super::tables as t;
use super::{CipherTable, HcaHeader, SAMPLES_PER_BLOCK, SAMPLES_PER_SUBFRAME};

const BANDS: usize = 128;
const SUBFRAMES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelType {
	Discrete = 0,
	StereoPrimary = 1,
	StereoSecondary = 2,
}

struct Channel {
	ty: ChannelType,
	coded_count: usize,
	scalefactors: [u8; BANDS],
	resolutions: [u8; BANDS],
	intensity: [u8; SUBFRAMES],
	gain: [f32; BANDS],
	spectra: [f32; BANDS],
	prev: [f32; BANDS],
	dct: [f32; BANDS],
	scratch: [f32; BANDS],
}

impl Channel {
	fn new(ty: ChannelType, coded_count: usize) -> Self {
		Self {
			ty,
			coded_count,
			scalefactors: [0; BANDS],
			resolutions: [0; BANDS],
			intensity: [0; SUBFRAMES],
			gain: [0.0; BANDS],
			spectra: [0.0; BANDS],
			prev: [0.0; BANDS],
			dct: [0.0; BANDS],
			scratch: [0.0; BANDS],
		}
	}
}

struct Decoder {
	cipher: Option<CipherTable>,
	channels: Vec<Channel>,
	total_band_count: usize,
	base_band_count: usize,
	stereo_band_count: usize,
	hfr_group_count: usize,
	bands_per_hfr_group: usize,
	noise_level: i32,
	evaluation_boundary: i32,
	block: Vec<u8>,
}

impl Decoder {
	fn new(header: &HcaHeader, keycode: Option<u64>, subkey: u16) -> Result<Self> {
		// Streams outside this envelope are refused.
		let resolution = |which, r| Error::unsupported(Format::Hca, format!("{which} resolution {r}"));
		ensure!(header.min_resolution == 1, resolution("minimum", header.min_resolution));
		ensure!(header.max_resolution == 15, resolution("maximum", header.max_resolution));
		ensure!(header.track_count >= 1, Error::malformed(Format::Hca, "zero track count"));
		ensure!(header.ms_stereo == 0, Error::unsupported(Format::Hca, "mid/side stereo"));

		let total = header.total_band_count as usize;
		let base = header.base_band_count as usize;
		let stereo = header.stereo_band_count as usize;
		let bphg = header.bands_per_hfr_group as usize;
		ensure!(
			total <= BANDS && base + stereo <= BANDS,
			Error::malformed(Format::Hca, "band counts exceed 128")
		);

		// ceil((total - stereo - base) / bands_per_hfr_group)
		let hfr_group_count = if bphg == 0 { 0 } else { (total - stereo - base).div_ceil(bphg) };

		let types = channel_types(
			header.channels as usize,
			header.track_count as usize,
			header.channel_config as usize,
			stereo > 0,
		);
		let channels = types
			.into_iter()
			.map(|ty| {
				// A secondary channel codes only the base bands.
				let coded = if ty == ChannelType::StereoSecondary { base } else { base + stereo };
				Channel::new(ty, coded)
			})
			.collect();

		let cipher = CipherTable::for_type(header.ciph_type, keycode.unwrap_or(0), subkey)?;
		let block = vec![0u8; header.block_size as usize];

		Ok(Self {
			cipher,
			channels,
			total_band_count: total,
			base_band_count: base,
			stereo_band_count: stereo,
			hfr_group_count,
			bands_per_hfr_group: bphg,
			noise_level: 0,
			evaluation_boundary: 0,
			block,
		})
	}

	// One block into `out`, `channels * 1024` interleaved samples
	fn decode_block(&mut self, encrypted: &[u8], out: &mut Vec<f32>) -> Result<()> {
		ensure!(
			encrypted.len() == self.block.len(),
			Error::truncated(Format::Hca, self.block.len(), encrypted.len())
		);

		// CRC over the ciphertext, then decrypt, then check sync
		ensure!(super::block_is_intact(encrypted), Error::ChecksumMismatch);
		self.block.copy_from_slice(encrypted);
		if let Some(c) = &self.cipher {
			c.apply(&mut self.block);
		}
		ensure!(
			self.block[0] == 0xff && self.block[1] == 0xff,
			Error::malformed(Format::Hca, "block sync is not 0xFFFF")
		);

		let block = std::mem::take(&mut self.block);
		let result = self.decode_decrypted(&block, out);
		self.block = block;
		result
	}

	fn decode_decrypted(&mut self, block: &[u8], out: &mut Vec<f32>) -> Result<()> {
		let mut br = BitReader::new(block);
		br.skip(16);
		self.unpack_frame_header(&mut br)?;

		let nch = self.channels.len();
		for sf in 0..SUBFRAMES {
			for ch in 0..nch {
				self.decode_spectrum(ch, &mut br)?;
			}
			self.reconstruct_high_frequency();
			self.apply_intensity_stereo(sf);
			let base = out.len();
			out.resize(base + nch * SAMPLES_PER_SUBFRAME as usize, 0.0);
			for ch in 0..nch {
				self.imdct(ch);
				let c = &mut self.channels[ch];
				window_overlap(&c.dct, &mut c.prev, &mut c.scratch);
				for (s, v) in c.scratch.iter().enumerate() {
					out[base + s * nch + ch] = *v;
				}
			}
		}
		Ok(())
	}

	fn unpack_frame_header(&mut self, br: &mut BitReader) -> Result<()> {
		self.noise_level = br.read(9) as i32;
		self.evaluation_boundary = br.read(7) as i32;

		let base = self.base_band_count;
		let stereo = self.stereo_band_count;
		let hfr_groups = self.hfr_group_count;

		for ch in self.channels.iter_mut() {
			unpack_scalefactors(br, ch)?;

			if ch.ty == ChannelType::StereoSecondary {
				// Peek 4 bits; 15 means "absent" and consumes nothing.
				let first = br.peek(4) as u8;
				ch.intensity[0] = first;
				if first < 15 {
					br.skip(4);
					for i in 1..SUBFRAMES {
						ch.intensity[i] = br.read(4) as u8;
					}
				}
			} else {
				for g in 0..hfr_groups {
					ch.scalefactors[base + stereo + g] = br.read(6) as u8;
				}
			}
		}

		let noise = (self.noise_level << 8) - self.evaluation_boundary;
		for ch in self.channels.iter_mut() {
			for i in 0..ch.coded_count {
				let sf = ch.scalefactors[i];
				let res = if sf == 0 {
					0u8
				} else {
					let v = ((noise + i as i32) >> 8) - ((5 * sf as i32) >> 1) + 1;
					if v < 0 {
						15
					} else if v >= 57 {
						1
					} else {
						t::SCALE_TO_RESOLUTION[v as usize]
					}
				};
				ch.resolutions[i] = res;
			}
			ch.resolutions[ch.coded_count..].fill(0);

			for i in 0..ch.coded_count {
				let sf = ch.scalefactors[i] as usize;
				let reason = || format!("scalefactor {sf} out of range");
				ensure!(sf < t::DEQUANT_SCALE.len(), Error::malformed(Format::Hca, reason()));
				ch.gain[i] = t::DEQUANT_SCALE[sf] * t::QUANT_STEP[ch.resolutions[i] as usize];
			}
		}
		Ok(())
	}

	// VLC-decode the quantized spectrum and scale by the gain.
	fn decode_spectrum(&mut self, ch_index: usize, br: &mut BitReader) -> Result<()> {
		let ch = &mut self.channels[ch_index];
		for i in 0..ch.coded_count {
			let res = ch.resolutions[i] as usize;
			let max_bits = t::QUANT_MAX_BITS[res] as u32;
			let code = br.peek(max_bits);
			let (value, consumed) = if res >= 8 {
				// Sign-magnitude; a zero magnitude leaves the sign bit unconsumed.
				let v = (code >> 1) as i32 * (1 - 2 * (code & 1) as i32);
				(v as f32, if v != 0 { max_bits } else { max_bits - 1 })
			} else {
				let idx = 16 * res + code as usize;
				(t::QUANT_VALUE[idx], t::QUANT_BITS[idx] as u32)
			};
			br.skip(consumed);
			ch.spectra[i] = value * ch.gain[i];
		}
		ch.spectra[ch.coded_count..].fill(0.0);
		Ok(())
	}

	// Rebuild the bands above the coded range by folding the coded
	// spectrum back down, scaled by a scalefactor difference.
	fn reconstruct_high_frequency(&mut self) {
		if self.bands_per_hfr_group == 0 {
			return;
		}
		let start = self.base_band_count + self.stereo_band_count;
		let total = self.total_band_count;
		for ch in self.channels.iter_mut() {
			if ch.ty == ChannelType::StereoSecondary {
				continue;
			}
			let mut src = start as i32 - 1;
			let mut dst = start;
			for g in 0..self.hfr_group_count {
				let mut n = 0;
				loop {
					if src < 0 || dst >= total {
						break;
					}
					n += 1;
					let hi = ch.scalefactors[start + g] as i32;
					let lo = ch.scalefactors[src as usize] as i32;
					ch.spectra[dst] = t::SCALE_CONVERSION[(hi - lo + 63) as usize] * ch.spectra[src as usize];
					src -= 1;
					dst += 1;
					if n >= self.bands_per_hfr_group {
						break;
					}
				}
			}
			if dst > 0 {
				ch.spectra[dst - 1] = 0.0;
			}
		}
	}

	// Pan the primary channel's upper bands across the pair.
	fn apply_intensity_stereo(&mut self, subframe: usize) {
		if self.stereo_band_count == 0 {
			return;
		}
		let (base, total) = (self.base_band_count, self.total_band_count);
		for i in 0..self.channels.len().saturating_sub(1) {
			if self.channels[i].ty != ChannelType::StereoPrimary {
				continue;
			}
			let ratio_l = t::INTENSITY_RATIO[self.channels[i + 1].intensity[subframe] as usize];
			let ratio_r = 2.0 - ratio_l;
			for band in base..total {
				let orig = self.channels[i].spectra[band];
				self.channels[i].spectra[band] = orig * ratio_l;
				self.channels[i + 1].spectra[band] = orig * ratio_r;
			}
		}
	}

	// The DCT proper. Leaves the result in `ch.dct`.
	fn imdct(&mut self, ch_index: usize) {
		let ch = &mut self.channels[ch_index];
		// Phase 1 ping-pongs between the spectra buffer, which it destroys, and a scratch buffer.
		let mut a = ch.spectra;
		let mut b = ch.scratch;

		let mut count1 = 1usize;
		let mut count2 = 64usize;
		for _ in 0..7 {
			let mut src = 0usize;
			for j in 0..count1 {
				let lo = j * 2 * count2;
				let hi = lo + count2;
				for k in 0..count2 {
					let x = a[src];
					let y = a[src + 1];
					src += 2;
					b[lo + k] = y + x;
					b[hi + k] = x - y;
				}
			}
			std::mem::swap(&mut a, &mut b);
			count2 >>= 1;
			count1 *= 2;
		}

		let mut count1 = 1usize;
		let mut count2 = BANDS;
		for stage in 0..7 {
			count2 >>= 1;
			count1 *= 2;
			let half = count1 >> 1;
			let tbl = 384 - 64 * stage;
			let mut q = 0usize;
			for o in 0..count2 {
				for k in 0..half {
					let x = a[o * count1 + k];
					let y = a[o * count1 + half + k];
					let ta = t::DCT_A[tbl + q];
					let tb = t::DCT_B[tbl + q];
					q += 1;
					b[o * count1 + k] = (x * tb) - (y * ta);
					b[(o + 1) * count1 - 1 - k] = (x * ta) + (y * tb);
				}
			}
			std::mem::swap(&mut a, &mut b);
		}
		ch.dct = a;
	}
}

// Window the DCT output and add the previous sub-frame's tail. `out` is written
// from the old `prev` before `prev` is replaced.
fn window_overlap(dct: &[f32; BANDS], prev: &mut [f32; BANDS], out: &mut [f32; BANDS]) {
	const HALF: usize = BANDS / 2;
	for k in 0..HALF {
		out[k] = t::IMDCT_WINDOW[k] * dct[HALF + k] + prev[k];
	}
	for j in 0..HALF {
		out[HALF + j] = t::IMDCT_WINDOW[HALF + j] * dct[BANDS - 1 - j] - prev[HALF + j];
	}
	for k in 0..HALF {
		prev[k] = t::IMDCT_WINDOW[BANDS - 1 - k] * dct[HALF - 1 - k];
	}
	for k in 0..HALF {
		prev[HALF + k] = t::IMDCT_WINDOW[HALF - 1 - k] * dct[k];
	}
}

// A 3-bit delta width, then a 6-bit seed and delta-coded remainder with
// an all-ones escape back to a literal 6-bit value.
fn unpack_scalefactors(br: &mut BitReader, ch: &mut Channel) -> Result<()> {
	let head = br.peek(9);
	let delta_bits = head >> 6;
	if delta_bits == 0 {
		br.skip(3);
		ch.scalefactors.fill(0);
		return Ok(());
	}
	br.skip(9);
	let mut value = (head & 0x3f) as u8;
	ch.scalefactors[0] = value;

	if delta_bits >= 6 {
		for i in 1..ch.coded_count {
			ch.scalefactors[i] = br.read(6) as u8;
		}
	} else {
		let escape = ((1u32 << delta_bits) - 1) as u8;
		for i in 1..ch.coded_count {
			let delta = br.read(delta_bits) as u8;
			value = if delta == escape {
				br.read(6) as u8
			} else {
				value.wrapping_add(delta).wrapping_sub(escape >> 1)
			};
			ch.scalefactors[i] = value;
		}
	}
	Ok(())
}

// Per-track channel role assignment
fn channel_types(
	channels: usize,
	track_count: usize,
	channel_config: usize,
	stereo: bool,
) -> Vec<ChannelType> {
	use ChannelType::*;
	let mut out = vec![Discrete; channels];
	if track_count == 0 {
		return out;
	}
	let per_track = channels / track_count;
	if !stereo || per_track == 1 {
		return out;
	}
	let pattern: &[ChannelType] = match per_track {
		2 => &[StereoPrimary, StereoSecondary],
		3 => &[StereoPrimary, StereoSecondary, Discrete],
		4 if channel_config != 0 => &[StereoPrimary, StereoSecondary, Discrete, Discrete],
		4 => &[StereoPrimary, StereoSecondary, StereoPrimary, StereoSecondary],
		5 if channel_config > 2 => &[StereoPrimary, StereoSecondary, Discrete, Discrete, Discrete],
		5 => &[StereoPrimary, StereoSecondary, Discrete, StereoPrimary, StereoSecondary],
		6 => &[StereoPrimary, StereoSecondary, Discrete, Discrete, StereoPrimary, StereoSecondary],
		7 => &[
			StereoPrimary,
			StereoSecondary,
			Discrete,
			Discrete,
			StereoPrimary,
			StereoSecondary,
			Discrete,
		],
		8 => &[
			StereoPrimary,
			StereoSecondary,
			Discrete,
			Discrete,
			StereoPrimary,
			StereoSecondary,
			StereoPrimary,
			StereoSecondary,
		],
		_ => return out,
	};
	for track in 0..track_count {
		for (i, ty) in pattern.iter().enumerate() {
			let idx = track * per_track + i;
			if idx < channels {
				out[idx] = *ty;
			}
		}
	}
	out
}

// A whole HCA stream, its encoder delay and padding trimmed
pub fn decode_stream(
	hca: &[u8],
	keycode: Option<u64>,
	subkey: u16,
	max_blocks: Option<u32>,
) -> Result<(HcaHeader, Vec<f32>)> {
	let header = HcaHeader::parse(hca)?;
	let block_size = header.block_size as usize;
	let data_offset = header.data_offset as usize;
	let blocks = max_blocks.map_or(header.block_count, |n| n.min(header.block_count));
	let needed = data_offset + blocks as usize * block_size;
	ensure!(hca.len() >= needed, Error::truncated(Format::Hca, needed, hca.len()));

	let nch = header.channels as usize;
	let mut dec = Decoder::new(&header, keycode, subkey)?;
	let mut raw = Vec::with_capacity(blocks as usize * SAMPLES_PER_BLOCK as usize * nch);
	for i in 0..blocks as usize {
		let off = data_offset + i * block_size;
		dec.decode_block(&hca[off..off + block_size], &mut raw)?;
	}

	// `encoder_delay` samples are dropped, then exactly `total_samples` kept.
	let skip = header.encoder_delay as usize * nch;
	let want = if max_blocks.is_some() {
		raw.len().saturating_sub(skip)
	} else {
		header.total_samples().max(0) as usize * nch
	};
	let end = (skip + want).min(raw.len());
	Ok((header, raw[skip.min(raw.len())..end].to_vec()))
}

#[cfg(test)]
mod tests {
	use super::ChannelType::*;
	use super::*;

	// The roles by channels per track. Streams without stereo bands never reach these.
	#[test]
	fn channel_roles() {
		let cases: &[(usize, usize, usize, &[ChannelType])] = &[
			(2, 1, 0, &[StereoPrimary, StereoSecondary]),
			(3, 1, 0, &[StereoPrimary, StereoSecondary, Discrete]),
			(4, 1, 0, &[StereoPrimary, StereoSecondary, StereoPrimary, StereoSecondary]),
			(4, 1, 1, &[StereoPrimary, StereoSecondary, Discrete, Discrete]),
			(5, 1, 0, &[StereoPrimary, StereoSecondary, Discrete, StereoPrimary, StereoSecondary]),
			(5, 1, 3, &[StereoPrimary, StereoSecondary, Discrete, Discrete, Discrete]),
			(
				6,
				1,
				0,
				&[StereoPrimary, StereoSecondary, Discrete, Discrete, StereoPrimary, StereoSecondary],
			),
			(
				8,
				1,
				0,
				&[
					StereoPrimary,
					StereoSecondary,
					Discrete,
					Discrete,
					StereoPrimary,
					StereoSecondary,
					StereoPrimary,
					StereoSecondary,
				],
			),
		];

		for &(channels, tracks, cfg, expected) in cases {
			let got = channel_types(channels, tracks, cfg, true);
			assert_eq!(got, expected, "channels={channels} tracks={tracks} cfg={cfg}");
		}
	}

	// All channels are discrete when there are no stereo bands.
	#[test]
	fn no_stereo_bands() {
		assert_eq!(channel_types(2, 1, 0, false), vec![Discrete, Discrete]);
		assert_eq!(channel_types(6, 1, 0, false), vec![Discrete; 6]);
	}

	#[test]
	fn roles_per_track() {
		assert_eq!(
			channel_types(4, 2, 0, true),
			vec![StereoPrimary, StereoSecondary, StereoPrimary, StereoSecondary]
		);
	}
}
