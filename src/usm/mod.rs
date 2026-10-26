// The USM container

pub mod ivf;
mod mask;

use std::path::Path;

use crate::error::{Error, Format, Result, ensure};

use crate::utf::Utf;
use mask::Masks;

#[derive(Debug, Clone)]
struct Chunk {
	signature: [u8; 4],
	payload_type: u8,
	payload: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
	pub filename: String,
	pub filesize: u64,
	pub chno: i64,
	pub stmid: u32,
	pub avbps: i64,
}

impl DirEntry {
	pub fn is_container(&self) -> bool {
		self.stmid == 0
	}
}

#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
	pub width: u32,
	pub height: u32,
	pub disp_width: u32,
	pub disp_height: u32,
	pub codec: u32,
	pub alpha_type: u32,
	pub total_frames: u32,
	pub framerate_n: u32,
	pub framerate_d: u32,
}

impl VideoInfo {
	pub fn codec_name(&self) -> &'static str {
		match self.codec {
			1 => "SofdecPrime",
			5 => "H264",
			9 => "VP9",
			_ => "unknown",
		}
	}

	pub fn stream_extension(&self) -> &'static str {
		match self.codec {
			1 => "m1v",
			5 => "h264",
			9 => "ivf",
			_ => "bin",
		}
	}

	pub fn fps(&self) -> f64 {
		if self.framerate_d == 0 { 0.0 } else { self.framerate_n as f64 / self.framerate_d as f64 }
	}
}

#[derive(Debug, Clone, Default)]
pub struct Streams {
	pub video: Vec<u8>,
	pub audio: Vec<u8>,
	pub alpha: Vec<u8>,
}

pub struct Usm {
	data: Vec<u8>,
	chunks: Vec<Chunk>,
	pub dir: Vec<DirEntry>,
	pub video: Option<VideoInfo>,
}

impl Usm {
	pub fn open(path: &Path) -> Result<Self> {
		let data =
			std::fs::read(path).map_err(|source| Error::Io { path: path.to_path_buf(), source })?;
		Self::parse(data)
	}

	pub fn parse(data: Vec<u8>) -> Result<Self> {
		ensure!(data.starts_with(b"CRID"), Error::malformed(Format::Usm, "no CRID header"));
		let mut chunks = Vec::new();
		let mut off = 0usize;
		while off < data.len() {
			ensure!(off + 32 <= data.len(), Error::truncated(Format::Usm, off + 32, data.len()));
			let signature: [u8; 4] = data[off..off + 4].try_into().unwrap();
			if &signature != b"CRID" && signature[0] != b'@' {
				let reason = format!("chunk at {off:#x}: bad signature {signature:02x?}");
				return Err(Error::malformed(Format::Usm, reason));
			}
			let size = be32(&data, off + 4) as usize;
			let header_size = be16(&data, off + 8) as usize;
			let footer_size = be16(&data, off + 10) as usize;
			let payload_type = data[off + 15];
			let reason =
				|| format!("chunk at {off:#x}: header {header_size} + footer {footer_size} exceed {size}");
			ensure!(
				header_size >= 24 && header_size + footer_size <= size,
				Error::malformed(Format::Usm, reason())
			);
			let end = off
				.checked_add(8)
				.and_then(|v| v.checked_add(size))
				.filter(|e| *e <= data.len())
				.ok_or(Error::truncated(Format::Usm, off.saturating_add(8 + size), data.len()))?;
			chunks.push(Chunk {
				signature,
				payload_type,
				payload: (off + 8 + header_size)..(end - footer_size),
			});
			off = end;
		}

		let mut dir = Vec::new();
		let mut video = None;
		for c in &chunks {
			if !matches!(c.payload_type, 1 | 3) {
				continue;
			}
			let pay = &data[c.payload.clone()];
			if pay.len() < 4 || &pay[0..4] != b"@UTF" {
				continue;
			}
			let utf = Utf::parse(pay)?;
			match utf.name.as_str() {
				"CRIUSF_DIR_STREAM" => {
					for row in 0..utf.rows.len() {
						dir.push(DirEntry {
							filename: utf.get_str(row, "filename").unwrap_or_default().to_string(),
							filesize: utf.get_i64(row, "filesize").unwrap_or(0) as u64,
							chno: utf.get_i64(row, "chno").unwrap_or(-1),
							stmid: utf.get_i64(row, "stmid").unwrap_or(0) as u32,
							avbps: utf.get_i64(row, "avbps").unwrap_or(0),
						});
					}
				}
				"VIDEO_HDRINFO" if video.is_none() => {
					let g = |n: &str| utf.get_i64(0, n).unwrap_or(0) as u32;
					video = Some(VideoInfo {
						width: g("width"),
						height: g("height"),
						disp_width: g("disp_width"),
						disp_height: g("disp_height"),
						codec: g("mpeg_codec"),
						alpha_type: g("alpha_type"),
						total_frames: g("total_frames"),
						framerate_n: g("framerate_n"),
						framerate_d: g("framerate_d"),
					});
				}
				_ => {}
			}
		}

		Ok(Self { data, chunks, dir, video })
	}

	pub fn demux(&self, keycode: Option<u64>) -> Streams {
		let masks = keycode.map(Masks::derive);
		let mut out = Streams::default();
		for c in &self.chunks {
			if c.payload_type != 0 {
				continue;
			}
			let target = match &c.signature {
				b"@SFV" => &mut out.video,
				b"@SFA" => &mut out.audio,
				b"@ALP" => &mut out.alpha,
				_ => continue,
			};
			let start = target.len();
			target.extend_from_slice(&self.data[c.payload.clone()]);
			if let Some(m) = &masks {
				let payload = &mut target[start..];
				match &c.signature {
					b"@SFA" => m.decrypt_audio(payload),
					_ => m.decrypt_video(payload),
				}
			}
		}
		out
	}

	pub fn verify(&self, streams: &Streams) -> Vec<(String, u64, u64)> {
		let mut bad = Vec::new();
		for e in self.dir.iter().filter(|e| !e.is_container()) {
			let actual = match &e.stmid.to_be_bytes() {
				b"@SFV" => streams.video.len(),
				b"@SFA" => streams.audio.len(),
				b"@ALP" => streams.alpha.len(),
				other => {
					bad.push((format!("{} (unknown stream {other:02x?})", e.filename), e.filesize, 0));
					continue;
				}
			} as u64;
			if actual != e.filesize {
				bad.push((e.filename.clone(), e.filesize, actual));
			}
		}
		bad
	}

	pub fn chunk_counts(&self) -> Vec<(String, usize)> {
		let mut acc: Vec<(String, usize)> = Vec::new();
		for c in &self.chunks {
			let sig = std::str::from_utf8(&c.signature).unwrap_or("????");
			let key = format!("{sig}:{}", c.payload_type);
			match acc.iter_mut().find(|(k, _)| *k == key) {
				Some((_, n)) => *n += 1,
				None => acc.push((key, 1)),
			}
		}
		acc
	}
}

fn be16(b: &[u8], o: usize) -> u16 {
	u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
	u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
