// The commands that write files: decoded WAVs, stored bitstreams and USM streams

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cri::acb::Acb;
use cri::afs2::Afs2;
use cri::usm::Usm;
use cri::{adx, hca};

use crate::explore::{Kind, load_awb, sniff};

// A bitstream decoded to interleaved samples, with its channel count and rate
fn decode_bitstream(b: &[u8], keycode: Option<u64>, subkey: u16) -> Result<(Vec<f32>, u16, u32)> {
	match sniff(b) {
		Kind::Hca => {
			let (h, pcm) = hca::decode_stream(b, keycode, subkey, None)?;
			Ok((pcm, u16::from(h.channels), h.sample_rate))
		}
		Kind::Adx => {
			let (h, pcm) = adx::decode_stream(b, keycode, subkey)?;
			Ok((pcm, u16::from(h.channels), h.sample_rate))
		}
		kind => bail!("{kind} is not an audio bitstream"),
	}
}

fn write_wav(path: &Path, pcm: &[f32], channels: u16, rate: u32, pcm16: bool) -> Result<()> {
	let (format, bits) = if pcm16 { (1u16, 16u16) } else { (3, 32) };
	let block = channels * bits / 8;
	let data_len = pcm.len() as u32 * u32::from(bits / 8);
	let mut out = Vec::with_capacity(44 + data_len as usize);
	out.extend_from_slice(b"RIFF");
	out.extend_from_slice(&(36 + data_len).to_le_bytes());
	out.extend_from_slice(b"WAVEfmt ");
	out.extend_from_slice(&16u32.to_le_bytes());
	out.extend_from_slice(&format.to_le_bytes());
	out.extend_from_slice(&channels.to_le_bytes());
	out.extend_from_slice(&rate.to_le_bytes());
	out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
	out.extend_from_slice(&block.to_le_bytes());
	out.extend_from_slice(&bits.to_le_bytes());
	out.extend_from_slice(b"data");
	out.extend_from_slice(&data_len.to_le_bytes());
	for &s in pcm {
		if pcm16 {
			out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
		} else {
			out.extend_from_slice(&s.to_le_bytes());
		}
	}
	let mut f =
		std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
	f.write_all(&out).with_context(|| format!("writing {}", path.display()))
}

pub fn decode(
	input: &Path,
	out: &Path,
	keycode: Option<u64>,
	subkey: u16,
	pcm16: bool,
) -> Result<()> {
	let bytes = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
	let (pcm, channels, rate) = decode_bitstream(&bytes, keycode, subkey)?;
	write_wav(out, &pcm, channels, rate, pcm16)?;
	let frames = pcm.len() / usize::from(channels.max(1));
	println!("wrote {}: {frames} frames, {channels}ch {rate} Hz", out.display());
	Ok(())
}

pub struct Extract {
	pub raw: bool,
	pub cue: Option<String>,
	pub keycode: Option<u64>,
	pub pcm16: bool,
}

// One bitstream to write, and the name to write it under
struct Item<'a> {
	name: String,
	bytes: &'a [u8],
	subkey: u16,
}

pub fn extract(
	input: &Path,
	awb: Option<std::path::PathBuf>,
	out: &Path,
	opts: &Extract,
) -> Result<()> {
	let bytes = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
	std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
	match sniff(&bytes) {
		Kind::Afs2 => {
			if opts.cue.is_some() {
				bail!("--cue needs an ACB; an AWB has no cue names");
			}
			let a = Afs2::parse(&bytes)?;
			let items = a.entries.iter().map(|e| Item {
				name: format!("{:05}", e.id),
				bytes: &bytes[e.offset..e.offset + e.len],
				subkey: a.subkey,
			});
			write_items(items.collect(), out, opts)
		}
		Kind::Utf => {
			let acb = Acb::from_bytes(bytes)?;
			let embedded = acb.internal_awb().transpose()?;
			let streaming = load_awb(input, awb)?;
			let streaming =
				streaming.as_ref().map(|b| Afs2::parse(b).map(|a| (a, b.as_slice()))).transpose()?;

			// Each waveform is named by the first cue that reaches it.
			let mut first_cue = HashMap::new();
			for c in acb.cues() {
				for w in c.waveforms {
					first_cue.entry(w).or_insert_with(|| c.name.clone());
				}
			}
			let filter = opts.cue.as_deref().map(str::to_lowercase);
			let mut items = Vec::new();
			let mut missing = 0;
			for w in acb.waveforms() {
				let cue = first_cue.get(&w.index).map_or("", String::as_str);
				if let Some(f) = &filter
					&& !cue.to_lowercase().contains(f)
				{
					continue;
				}
				let archive = if w.streaming { streaming.as_ref() } else { embedded.as_ref() };
				let Some((a, buf)) = archive else {
					missing += 1;
					continue;
				};
				let Some(e) = a.entries.iter().find(|e| i64::from(e.id) == w.awb_id) else {
					missing += 1;
					continue;
				};
				let name = if cue.is_empty() {
					format!("{:04}", w.index)
				} else {
					format!("{:04}_{}", w.index, file_safe(cue))
				};
				items.push(Item { name, bytes: &buf[e.offset..e.offset + e.len], subkey: a.subkey });
			}
			if missing > 0 {
				println!(
					"{missing} waveform(s) skipped: their archive is not loaded or lacks them; --awb names the streaming one"
				);
			}
			write_items(items, out, opts)
		}
		kind => bail!("{} is {kind}, not an ACB or AWB", input.display()),
	}
}

fn write_items(items: Vec<Item>, out: &Path, opts: &Extract) -> Result<()> {
	let mut failed = 0;
	for item in &items {
		let result = if opts.raw {
			let ext = match sniff(item.bytes) {
				Kind::Hca => "hca",
				Kind::Adx => "adx",
				_ => "bin",
			};
			let path = out.join(format!("{}.{ext}", item.name));
			std::fs::write(&path, item.bytes).with_context(|| format!("writing {}", path.display()))
		} else {
			decode_bitstream(item.bytes, opts.keycode, item.subkey).and_then(|(pcm, ch, rate)| {
				write_wav(&out.join(format!("{}.wav", item.name)), &pcm, ch, rate, opts.pcm16)
			})
		};
		if let Err(e) = result {
			failed += 1;
			println!("{}: {e:#}", item.name);
		}
	}
	println!("wrote {} of {} to {}", items.len() - failed, items.len(), out.display());
	if failed > 0 {
		bail!("{failed} failed");
	}
	Ok(())
}

// Cue names are ASCII identifiers in practice; anything a file system could refuse becomes `_`.
fn file_safe(name: &str) -> String {
	name.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
}

pub fn demux(path: &Path, out: &Path, keycode: Option<u64>) -> Result<()> {
	let u = Usm::open(path)?;
	let streams = u.demux(keycode);
	std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
	let stem = path.file_stem().map_or("usm".into(), |s| s.to_string_lossy().into_owned());
	let video_ext = u.video.map_or("bin", |v| v.stream_extension());
	let audio_ext = match sniff(&streams.audio) {
		Kind::Hca => "hca",
		Kind::Adx => "adx",
		_ => "bin",
	};
	for (bytes, name) in [
		(&streams.video, format!("{stem}.{video_ext}")),
		(&streams.alpha, format!("{stem}.alpha.{video_ext}")),
		(&streams.audio, format!("{stem}.{audio_ext}")),
	] {
		if bytes.is_empty() {
			continue;
		}
		let p = out.join(name);
		std::fs::write(&p, bytes).with_context(|| format!("writing {}", p.display()))?;
		println!("wrote {} ({} bytes)", p.display(), bytes.len());
	}
	for (name, want, got) in u.verify(&streams) {
		println!("warning: {name} should be {want} bytes and is {got}");
	}
	if keycode.is_none() {
		println!("no keycode given: a masked USM's streams are written masked");
	}
	Ok(())
}
