// `extract`: decoded WAVs, stored bitstreams and USM streams

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cri::acb::Acb;
use cri::afs2::Afs2;
use cri::usm::Usm;
use cri::{adx, hca};

use crate::explore::{Kind, load_awb, matching_cues, sniff};

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

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
	std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn extension(b: &[u8]) -> &'static str {
	match sniff(b) {
		Kind::Hca => "hca",
		Kind::Adx => "adx",
		_ => "bin",
	}
}

pub struct Extract {
	pub patterns: Vec<String>,
	pub out: Option<PathBuf>,
	pub raw: bool,
	pub subkey: Option<u16>,
	pub awb: Option<PathBuf>,
	pub keycode: Option<u64>,
	pub pcm16: bool,
}

// One bitstream to write, and the name to write it under
struct Item<'a> {
	name: String,
	bytes: &'a [u8],
	subkey: u16,
}

pub fn extract(input: &Path, o: Extract) -> Result<()> {
	let bytes = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
	let kind = sniff(&bytes);
	let acb = kind == Kind::Utf;
	let loose = matches!(kind, Kind::Hca | Kind::Adx);
	let refused = [
		(!o.patterns.is_empty() && !acb, "a cue pattern"),
		(o.awb.is_some() && !acb, "--awb"),
		(o.subkey.is_some() && !loose, "--subkey"),
		(o.raw && loose, "--raw"),
	];
	if let Some((_, flag)) = refused.iter().find(|(r, _)| *r) {
		bail!("{flag} does not apply to {}, which is {kind}", input.display());
	}

	// A loose bitstream goes beside itself; a container, into a directory named after it.
	let out = match &o.out {
		Some(out) => out.clone(),
		None if loose => input.parent().map_or_else(PathBuf::new, Path::to_path_buf),
		None => {
			let dir = input.with_extension("");
			if dir == input {
				bail!("{} has no extension to drop for a directory name; give --out", input.display());
			}
			dir
		}
	};
	let stem = input.file_stem().map_or("out".into(), |s| s.to_string_lossy().into_owned());

	match kind {
		Kind::Hca | Kind::Adx => {
			let item = Item { name: stem, bytes: &bytes, subkey: o.subkey.unwrap_or(0) };
			write_items(vec![item], &out, &o)
		}
		Kind::Afs2 => {
			let a = Afs2::parse(&bytes)?;
			let items = a.entries.iter().map(|e| Item {
				name: format!("{:05}", e.id),
				bytes: &bytes[e.offset..e.offset + e.len],
				subkey: a.subkey,
			});
			write_items(items.collect(), &out, &o)
		}
		Kind::Utf => {
			let acb = Acb::from_bytes(bytes)?;
			if !acb.root().columns.iter().any(|c| c.name == "CueTable") {
				bail!("{} is an @UTF file without cues, not an ACB", input.display());
			}
			let embedded = acb.internal_awb().transpose()?;
			let streaming = load_awb(input, o.awb.clone())?;
			let streaming =
				streaming.as_ref().map(|b| Afs2::parse(b).map(|a| (a, b.as_slice()))).transpose()?;

			// Each waveform is named by the first cue that reaches it.
			let mut first_cue = HashMap::new();
			for (_, c) in matching_cues(&acb, &o.patterns)? {
				for w in c.waveforms {
					first_cue.entry(w).or_insert_with(|| c.name.clone());
				}
			}
			let mut items = Vec::new();
			let mut missing = 0;
			for w in acb.waveforms() {
				let cue = match first_cue.get(&w.index) {
					Some(c) => c.as_str(),
					None if o.patterns.is_empty() => "",
					None => continue,
				};
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
			write_items(items, &out, &o)
		}
		Kind::Usm => usm(Usm::parse(bytes)?, &stem, &out, &o),
		Kind::Unknown => bail!("{} is not a CRI file", input.display()),
	}
}

// Created once there is something to write, so that a refused input leaves nothing behind
fn create_dir(out: &Path) -> Result<()> {
	if out.as_os_str().is_empty() {
		return Ok(());
	}
	std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))
}

fn write_items(items: Vec<Item>, out: &Path, o: &Extract) -> Result<()> {
	create_dir(out)?;
	let mut failed = 0;
	for item in &items {
		let result = if o.raw {
			write_file(&out.join(format!("{}.{}", item.name, extension(item.bytes))), item.bytes)
		} else {
			decode_bitstream(item.bytes, o.keycode, item.subkey).and_then(|(pcm, ch, rate)| {
				write_wav(&out.join(format!("{}.wav", item.name)), &pcm, ch, rate, o.pcm16)
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

// The video and alpha streams as stored; the audio decoded unless `raw`, and as stored when it
// does not decode.
fn usm(u: Usm, stem: &str, out: &Path, o: &Extract) -> Result<()> {
	create_dir(out)?;
	let streams = u.demux(o.keycode);
	let video_ext = u.video.map_or("bin", |v| v.stream_extension());
	for (bytes, name) in [
		(&streams.video, format!("{stem}.{video_ext}")),
		(&streams.alpha, format!("{stem}.alpha.{video_ext}")),
	] {
		if !bytes.is_empty() {
			let p = out.join(name);
			write_file(&p, bytes)?;
			println!("wrote {} ({} bytes)", p.display(), bytes.len());
		}
	}
	if !streams.audio.is_empty() {
		let decoded = if o.raw {
			None
		} else {
			match decode_bitstream(&streams.audio, o.keycode, 0) {
				Ok(d) => Some(d),
				Err(e) => {
					println!("audio does not decode, so it is written as stored: {e:#}");
					None
				}
			}
		};
		match decoded {
			Some((pcm, ch, rate)) => {
				let p = out.join(format!("{stem}.wav"));
				write_wav(&p, &pcm, ch, rate, o.pcm16)?;
				println!("wrote {} ({ch}ch {rate} Hz)", p.display());
			}
			None => {
				let p = out.join(format!("{stem}.{}", extension(&streams.audio)));
				write_file(&p, &streams.audio)?;
				println!("wrote {} ({} bytes)", p.display(), streams.audio.len());
			}
		}
	}
	for (name, want, got) in u.verify(&streams) {
		println!("warning: {name} should be {want} bytes and is {got}");
	}
	if o.keycode.is_none() {
		println!("no keycode given: a masked USM's streams are written masked");
	}
	Ok(())
}
