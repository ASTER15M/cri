// The read-only commands: what files are, their tables, cues, waveforms, buses and streams

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cri::acb::program::{
	ActionEvent, ActionTarget, CueParam, CueSheet, NestedSequence, ProgramTrack, SequenceType,
};
use cri::acb::{Acb, AwbSource};
use cri::acf::Acf;
use cri::adx::AdxHeader;
use cri::afs2::Afs2;
use cri::hca::HcaHeader;
use cri::usm::Usm;
use cri::utf::{Utf, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
	Utf,
	Afs2,
	Usm,
	Hca,
	Adx,
	Unknown,
}

impl std::fmt::Display for Kind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::Utf => "@UTF",
			Self::Afs2 => "AWB",
			Self::Usm => "USM",
			Self::Hca => "HCA",
			Self::Adx => "ADX",
			Self::Unknown => "unknown",
		})
	}
}

pub fn sniff(b: &[u8]) -> Kind {
	let masked: Vec<u8> = b.iter().take(4).map(|c| c & 0x7f).collect();
	match b {
		_ if b.starts_with(b"@UTF") => Kind::Utf,
		_ if b.starts_with(b"AFS2") => Kind::Afs2,
		_ if b.starts_with(b"CRID") => Kind::Usm,
		_ if masked == b"HCA\0" => Kind::Hca,
		[0x80, 0x00, ..] => Kind::Adx,
		_ => Kind::Unknown,
	}
}

// A bitstream's own header in one line
pub fn describe(b: &[u8]) -> String {
	match sniff(b) {
		Kind::Hca => match HcaHeader::parse(b) {
			Ok(h) => {
				let mut s = format!(
					"HCA {}.{} {}ch {} Hz {:.2} s, cipher {}",
					h.version >> 8,
					h.version & 0xff,
					h.channels,
					h.sample_rate,
					h.total_samples() as f64 / f64::from(h.sample_rate.max(1)),
					h.ciph_type
				);
				if let Some(l) = h.loop_info {
					let at = |n: i64| n as f64 / f64::from(h.sample_rate.max(1));
					s += &format!(", loop {:.3}..{:.3} s", at(l.start_sample(&h)), at(l.end_sample(&h)));
				}
				s
			}
			Err(e) => format!("HCA, unreadable: {e}"),
		},
		Kind::Adx => match AdxHeader::parse(b) {
			Ok(h) => {
				let mut s = format!(
					"ADX v{} {}ch {} Hz {:.2} s, encryption type {}",
					h.version,
					h.channels,
					h.sample_rate,
					f64::from(h.sample_count) / f64::from(h.sample_rate.max(1)),
					h.encoding_type
				);
				if let Some(l) = h.loop_info {
					let at = |n: u32| f64::from(n) / f64::from(h.sample_rate.max(1));
					s += &format!(", loop {:.3}..{:.3} s", at(l.start_sample), at(l.end_sample));
				}
				s
			}
			Err(e) => format!("ADX, unreadable: {e}"),
		},
		Kind::Unknown => format!("unknown, starts {}", hex(&b[..b.len().min(8)])),
		kind => kind.to_string(),
	}
}

fn hex(b: &[u8]) -> String {
	b.iter().map(|x| format!("{x:02x}")).collect()
}

fn read(path: &Path) -> Result<Vec<u8>> {
	std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

// The given AWB, or the `.awb` beside the ACB when there is one
pub fn load_awb(acb: &Path, awb: Option<PathBuf>) -> Result<Option<Vec<u8>>> {
	match awb {
		Some(p) => read(&p).map(Some),
		None => {
			let p = acb.with_extension("awb");
			Ok(if p.is_file() { Some(read(&p)?) } else { None })
		}
	}
}

fn open_sheet<'a>(acb: &'a Acb, awb: Option<Vec<u8>>) -> Result<CueSheet<'a>> {
	let sheet = CueSheet::open(acb)?;
	Ok(match awb {
		Some(b) => sheet.with_external_awb(b)?,
		None => sheet,
	})
}

fn seconds(ms: i64) -> String {
	if ms < 0 { "indeterminate".into() } else { format!("{:.3} s", ms as f64 / 1000.0) }
}

// The CRI files among `paths`, one line each, or `summarize`'s lines with `long`. A directory lists
// the files in it that read as CRI files; a file named outright is always listed.
pub fn ls(paths: &[PathBuf], recursive: bool, long: bool) -> Result<()> {
	let here = [PathBuf::from(".")];
	let paths = if paths.is_empty() { &here[..] } else { paths };
	for (n, path) in paths.iter().enumerate() {
		let files = if path.is_dir() {
			if paths.len() > 1 {
				println!(
					"{}{}:",
					if n > 0 {
						"
"
					} else {
						""
					},
					path.display()
				);
			}
			let mut files = Vec::new();
			walk(path, recursive, &mut files);
			files.sort();
			files
				.into_iter()
				.map(|f| {
					let name = f.strip_prefix(path).unwrap_or(&f).display().to_string();
					(if f.is_dir() { format!("{name}/") } else { name }, f, false)
				})
				.collect()
		} else {
			vec![(path.display().to_string(), path.clone(), true)]
		};
		let lines: Vec<(String, Vec<String>)> = files
			.into_iter()
			.filter_map(|(name, path, named)| {
				let lines = if long { long_lines(&path, named) } else { line(&path, named) };
				lines.map(|l| (name, l))
			})
			.collect();
		let width = lines.iter().map(|(name, _)| name.chars().count()).max().unwrap_or(0);
		for (i, (name, l)) in lines.iter().enumerate() {
			if long {
				if i > 0 {
					println!();
				}
				println!("{name}");
				l.iter().for_each(|l| println!("  {l}"));
			} else if let [l] = &l[..] {
				println!("{name:<width$}  {l}");
			} else {
				println!("{name}");
			}
		}
	}
	Ok(())
}

// Every file under `dir`, or without `recursive` its files and directories; a directory that cannot
// be read is reported and skipped.
fn walk(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
	let entries = match std::fs::read_dir(dir) {
		Ok(e) => e,
		Err(e) => return eprintln!("{}: {e}", dir.display()),
	};
	for e in entries.flatten() {
		let path = e.path();
		match e.file_type() {
			// Linked directories are not followed, as `ls -R` does not.
			Ok(t) if t.is_dir() => {
				if recursive {
					walk(&path, recursive, out);
				} else {
					out.push(path);
				}
			}
			Ok(_) if path.is_file() => out.push(path),
			_ => {}
		}
	}
}

// Enough of a file to tell what it is and read the headers of an HCA, ADX, AWB or USM
const HEAD: u64 = 64 * 1024;

fn read_head(path: &Path) -> Result<Vec<u8>> {
	use std::io::Read as _;
	let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
	let mut head = Vec::new();
	f.take(HEAD).read_to_end(&mut head).with_context(|| format!("reading {}", path.display()))?;
	Ok(head)
}

// Whether a file found in a directory is listed. `0x80 0x00` starts many files that are not ADX.
fn is_cri(head: &[u8]) -> bool {
	match sniff(head) {
		Kind::Unknown => false,
		Kind::Adx => AdxHeader::parse(head).is_ok(),
		_ => true,
	}
}

fn line(path: &Path, named: bool) -> Option<Vec<String>> {
	if path.is_dir() {
		return Some(Vec::new());
	}
	let head = match read_head(path) {
		Ok(h) => h,
		Err(e) => return Some(vec![format!("unreadable: {e:#}")]),
	};
	if !named && !is_cri(&head) {
		return None;
	}
	let kind = sniff(&head);
	Some(vec![brief(path, &head).unwrap_or_else(|e| format!("{kind}, unreadable: {e:#}"))])
}

fn long_lines(path: &Path, named: bool) -> Option<Vec<String>> {
	if path.is_dir() {
		return Some(Vec::new());
	}
	if !named && !read_head(path).is_ok_and(|h| is_cri(&h)) {
		return None;
	}
	Some(match read(path).and_then(|b| summarize(path, b)) {
		Ok(lines) => lines,
		Err(e) => vec![format!("unreadable: {e:#}")],
	})
}

// A file in one line, from its first `HEAD` bytes; an ACB or ACF is read whole.
fn brief(path: &Path, head: &[u8]) -> Result<String> {
	Ok(match sniff(head) {
		Kind::Utf => {
			let bytes = read(path)?;
			let root = Utf::parse(&bytes)?;
			let has = |c: &str| root.columns.iter().any(|col| col.name == c);
			if has("CueTable") {
				let acb = Acb::from_bytes(bytes)?;
				let cues = acb.table("CueTable").transpose()?.map_or(0, |t| t.rows.len());
				let waves = acb.waveforms();
				let missing = waves.iter().any(|w| w.streaming) && !path.with_extension("awb").is_file();
				format!(
					"ACB {:?}, {cues} cue(s), {} waveform(s){}",
					acb.name,
					waves.len(),
					if missing { ", streaming AWB missing" } else { "" }
				)
			} else if has("DspSettingTable") || has("CategoryTable") {
				let acf = Acf::from_bytes(&bytes)?;
				format!(
					"ACF {:?}, {} categories, {} DSP setting(s)",
					acf.name,
					acf.categories.len(),
					acf.dsp_settings.len()
				)
			} else {
				format!("@UTF table {:?}, {} row(s)", root.name, root.rows.len())
			}
		}
		// The count and subkey sit in the 16-byte header; `Afs2::parse` needs the whole archive.
		Kind::Afs2 if head.len() >= 16 => {
			let count = u32::from_le_bytes(head[8..12].try_into().unwrap());
			let subkey = u16::from_le_bytes(head[14..16].try_into().unwrap());
			format!("AWB, {count} entries, subkey {subkey}")
		}
		Kind::Usm => {
			let u = Usm::parse(usm_head(head).to_vec())?;
			let audio = u.dir.iter().any(|e| &e.stmid.to_be_bytes() == b"@SFA");
			let audio = if audio { ", with audio" } else { "" };
			match u.video {
				Some(v) => format!(
					"USM, {} {}x{} at {:.3} fps, {} frames{}{audio}",
					v.codec_name(),
					v.width,
					v.height,
					v.fps(),
					v.total_frames,
					if v.alpha_type != 0 { ", with alpha" } else { "" }
				),
				None => format!("USM, no video header in its first {HEAD} bytes{audio}"),
			}
		}
		_ => describe(head),
	})
}

// The whole chunks a USM's head holds, so that its headers parse without its streams
fn usm_head(b: &[u8]) -> &[u8] {
	let mut end = 0;
	while let Some(size) = b.get(end + 4..end + 8) {
		let next = end + 8 + u32::from_be_bytes(size.try_into().unwrap()) as usize;
		if next > b.len() {
			break;
		}
		end = next;
	}
	&b[..end]
}

fn summarize(path: &Path, bytes: Vec<u8>) -> Result<Vec<String>> {
	let len = bytes.len();
	let mut out = Vec::new();
	match sniff(&bytes) {
		Kind::Utf => {
			let root = Utf::parse(&bytes)?;
			let has = |c: &str| root.columns.iter().any(|col| col.name == c);
			let version = root.get_i64(0, "Version").map(|v| format!("{v:#010x}")).unwrap_or_default();
			if has("CueTable") {
				let acb = Acb::from_bytes(bytes)?;
				let waves = acb.waveforms();
				out.push(format!("ACB {:?}, version {version}, {len} bytes", acb.name));
				let cues = acb.table("CueTable").transpose()?.map_or(0, |t| t.rows.len());
				out.push(format!("{cues} cue(s), {} waveform(s)", waves.len()));
				let mut codecs = BTreeMap::<String, usize>::new();
				for w in &waves {
					let at = if w.streaming { "streaming" } else { "in memory" };
					*codecs.entry(format!("{} {at}", w.codec_name())).or_default() += 1;
				}
				for (codec, n) in codecs {
					out.push(format!("  {n:>5} {codec}"));
				}
				if let Some(awb) = acb.internal_awb().transpose()? {
					out.push(format!(
						"embedded AWB: {} entries, subkey {}",
						awb.0.entries.len(),
						awb.0.subkey
					));
				}
				if waves.iter().any(|w| w.streaming) {
					let p = path.with_extension("awb");
					let state = if p.is_file() { "beside it" } else { "not beside it" };
					out.push(format!("streaming AWB {} is {state}", p.display()));
				}
			} else if has("DspSettingTable") || has("CategoryTable") {
				let acf = Acf::from_bytes(&bytes)?;
				out.push(format!(
					"ACF {:?}, version {:#010x} ({}), {len} bytes",
					acf.name, acf.version, acf.version_string
				));
				out.push(format!(
					"{} categories, {} DSP setting(s) (default {:?}), {} global AISAC(s), {} REACT rule(s)",
					acf.categories.len(),
					acf.dsp_settings.len(),
					acf.default_dsp_setting,
					acf.global_aisacs.len(),
					acf.reacts.len()
				));
			} else {
				out.push(format!(
					"@UTF table {:?}: {} column(s), {} row(s), {len} bytes",
					root.name,
					root.columns.len(),
					root.rows.len()
				));
			}
		}
		Kind::Afs2 => {
			let a = Afs2::parse(&bytes)?;
			out.push(format!(
				"AWB: {} entries, alignment {}, subkey {}, {len} bytes",
				a.entries.len(),
				a.align,
				a.subkey
			));
			let mut kinds = BTreeMap::<String, usize>::new();
			for e in &a.entries {
				*kinds.entry(format!("{}", sniff(&bytes[e.offset..e.offset + e.len]))).or_default() += 1;
			}
			for (k, n) in kinds {
				out.push(format!("  {n:>5} {k}"));
			}
		}
		Kind::Usm => {
			let u = Usm::parse(bytes)?;
			out.push(format!("USM, {len} bytes"));
			if let Some(v) = u.video {
				out.push(format!(
					"video {} {}x{} at {:.3} fps, {} frames{}",
					v.codec_name(),
					v.width,
					v.height,
					v.fps(),
					v.total_frames,
					if v.alpha_type != 0 { ", with alpha" } else { "" }
				));
			}
			for e in u.dir.iter().filter(|e| !e.is_container()) {
				out.push(format!("stream {} {:?}, {} bytes", stream_name(e.stmid), e.filename, e.filesize));
			}
		}
		Kind::Hca | Kind::Adx | Kind::Unknown => out.push(format!("{}, {len} bytes", describe(&bytes))),
	}
	Ok(out)
}

fn stream_name(stmid: u32) -> String {
	String::from_utf8_lossy(&stmid.to_be_bytes()).into_owned()
}

// `A/B`: table `B` in the first row of table `A`, itself in the root's row
fn find_table(root: &Utf, path: &str) -> Result<Utf> {
	let mut t = root.clone();
	for step in path.split('/') {
		t = match t.get_table(0, step) {
			Some(r) => r.with_context(|| format!("{step} is not a @UTF table"))?,
			None if t.columns.iter().any(|c| c.name == step) => bail!("{step} is empty"),
			None => bail!("{} has no column {step}", t.name),
		};
	}
	Ok(t)
}

pub fn utf(
	path: &Path,
	table: Option<&str>,
	schema: bool,
	columns: &[String],
	row: Option<usize>,
	limit: usize,
) -> Result<()> {
	let bytes = read(path)?;
	let root =
		Utf::parse(&bytes).with_context(|| format!("{} is not a @UTF file", path.display()))?;
	let Some(table) = table else {
		if schema {
			print_schema(&root);
			return Ok(());
		}
		return print_tree(&root);
	};
	let t = find_table(&root, table)?;
	if schema {
		print_schema(&t);
		return Ok(());
	}

	let rows: Vec<usize> = match row {
		Some(r) if r >= t.rows.len() => bail!("{} has {} row(s)", t.name, t.rows.len()),
		Some(r) => vec![r],
		None => (0..t.rows.len()).take(if limit == 0 { usize::MAX } else { limit }).collect(),
	};
	println!("{}: {} row(s)", t.name, t.rows.len());
	if columns.is_empty() {
		for &r in &rows {
			println!("[{r}]");
			for (c, v) in t.columns.iter().zip(&t.rows[r]) {
				println!("  {:<28} {}", c.name, cell(v));
			}
		}
	} else {
		for c in columns {
			if !t.columns.iter().any(|col| &col.name == c) {
				bail!("{} has no column {c}", t.name);
			}
		}
		let cells: Vec<Vec<String>> = rows
			.iter()
			.map(|&r| columns.iter().map(|c| t.get(r, c).map_or("-".into(), cell)).collect())
			.collect();
		let widths: Vec<usize> = (0..columns.len())
			.map(|i| cells.iter().map(|row| row[i].len()).chain([columns[i].len()]).max().unwrap_or(0))
			.collect();
		let line = |r: String, values: &[String]| {
			let values: Vec<String> =
				values.iter().zip(&widths).map(|(v, w)| format!("{v:>w$}")).collect();
			println!("{r:>5}  {}", values.join("  "));
		};
		line("row".into(), columns);
		for (&r, values) in rows.iter().zip(&cells) {
			line(r.to_string(), values);
		}
	}
	if rows.len() < t.rows.len() && row.is_none() {
		println!("... {} more row(s); --limit 0 prints all", t.rows.len() - rows.len());
	}
	Ok(())
}

fn cell(v: &Value) -> String {
	match v {
		Value::Data(d) if d.starts_with(b"@UTF") => match Utf::parse(d) {
			Ok(t) => format!("<table {:?}, {} row(s)>", t.name, t.rows.len()),
			Err(_) => v.to_string(),
		},
		v => v.to_string(),
	}
}

fn print_schema(t: &Utf) {
	println!("{}: {} column(s), {} row(s)", t.name, t.columns.len(), t.rows.len());
	for c in &t.columns {
		println!("  {:<28} {}", c.name, type_name(c.type_id));
	}
}

fn type_name(id: u8) -> &'static str {
	match id {
		0x00 => "u8",
		0x01 => "i8",
		0x02 => "u16",
		0x03 => "i16",
		0x04 => "u32",
		0x05 => "i32",
		0x06 => "u64",
		0x07 => "i64",
		0x08 => "f32",
		0x09 => "f64",
		0x0a => "string",
		0x0b => "data",
		_ => "?",
	}
}

// The root's scalars and blobs, then every table it holds, with the tables in their first rows
// indented under them
fn print_tree(root: &Utf) -> Result<()> {
	println!("{}: {} column(s), {} row(s)", root.name, root.columns.len(), root.rows.len());
	for (c, v) in root.columns.iter().zip(root.rows.first().into_iter().flatten()) {
		if !matches!(v, Value::Data(_)) {
			println!("  {:<34} {v}", c.name);
		}
	}
	println!();
	fn tables(t: &Utf, depth: usize) {
		let Some(row) = t.rows.first() else { return };
		for (c, v) in t.columns.iter().zip(row) {
			let Value::Data(d) = v else { continue };
			let pad = "  ".repeat(depth + 1);
			let width = 34 - 2 * depth;
			match Utf::parse(d) {
				Ok(sub) if d.starts_with(b"@UTF") => {
					println!("{pad}{:<width$} {:>6} row(s)", c.name, sub.rows.len());
					tables(&sub, depth + 1);
				}
				_ if depth > 0 => {}
				_ if d.is_empty() => println!("{pad}{:<width$}  empty", c.name),
				_ => println!("{pad}{:<width$} {:>6} bytes, {}", c.name, d.len(), describe(d)),
			}
		}
	}
	tables(root, 0);
	Ok(())
}

pub struct Info {
	pub patterns: Vec<String>,
	pub cues: bool,
	pub waves: bool,
	pub full: bool,
	pub graphs: bool,
	pub awb: Option<PathBuf>,
}

// Every section of a file, or those the flags name. Flags that do not apply to the file refuse it.
pub fn info(path: &Path, o: Info) -> Result<()> {
	let bytes = read(path)?;
	let what = match sniff(&bytes) {
		Kind::Utf => {
			let root = Utf::parse(&bytes)?;
			let has = |c: &str| root.columns.iter().any(|col| col.name == c);
			if has("CueTable") {
				return acb_info(path, bytes, o);
			}
			if has("DspSettingTable") || has("CategoryTable") {
				refuse(&o, "an ACF", false, true)?;
				return acf(&Acf::from_bytes(&bytes)?, o.graphs);
			}
			"a bare @UTF table, which `cri utf` reads"
		}
		Kind::Afs2 => {
			refuse(&o, "an AWB", true, false)?;
			return awb(&bytes);
		}
		Kind::Usm => {
			refuse(&o, "a USM", false, false)?;
			let len = bytes.len();
			return usm(&Usm::parse(bytes)?, len);
		}
		Kind::Hca => "an HCA bitstream",
		Kind::Adx => "an ADX bitstream",
		Kind::Unknown => "not a CRI file",
	};
	refuse(&o, what, false, false)?;
	println!("{}", summarize(path, bytes)?.join("\n  "));
	Ok(())
}

fn refuse(o: &Info, what: &str, waves: bool, graphs: bool) -> Result<()> {
	let flags = [
		(!o.patterns.is_empty(), "a cue pattern"),
		(o.cues, "--cues"),
		(o.full, "--full"),
		(o.waves && !waves, "--waves"),
		(o.graphs && !graphs, "--graphs"),
	];
	match flags.iter().find(|(given, _)| *given) {
		Some((_, flag)) => bail!("{flag} does not apply to {what}"),
		None => Ok(()),
	}
}

fn acb_info(path: &Path, bytes: Vec<u8>, o: Info) -> Result<()> {
	let full = o.full || o.graphs;
	let all = !(o.cues || full || o.waves);
	let acb = Acb::from_bytes(bytes)?;

	// A number matches the cue with that ID; anything else, the cues whose names contain it.
	let patterns: Vec<(Option<i64>, String)> =
		o.patterns.iter().map(|p| (p.parse().ok(), p.to_lowercase())).collect();
	let cues: Vec<_> = acb
		.cues()
		.into_iter()
		.enumerate()
		.filter(|(_, c)| {
			let name = c.name.to_lowercase();
			patterns.is_empty()
				|| patterns.iter().any(|(id, p)| match id {
					Some(id) => c.id == *id,
					None => name.contains(p.as_str()),
				})
		})
		.collect();
	if cues.is_empty() && !patterns.is_empty() {
		bail!("no cue matches {}", o.patterns.join(", "));
	}
	if all {
		println!("{}\n", summarize(path, acb.buffer().to_vec())?.join("\n  "));
	}

	// The waveform lines borrow the streaming AWB, which the cue sheet then takes.
	let streaming = load_awb(path, o.awb)?;
	let waves = (all || o.waves)
		.then(|| {
			let only: Option<BTreeSet<usize>> = (!patterns.is_empty())
				.then(|| cues.iter().flat_map(|(_, c)| c.waveforms.iter().copied()).collect());
			wave_lines(&acb, streaming.as_deref(), only.as_ref())
		})
		.transpose()?;
	let sheet = open_sheet(&acb, streaming)?;
	let indices: Vec<usize> = cues.iter().map(|(i, _)| *i).collect();
	let show_cues = all || o.cues || full;
	if show_cues {
		println!("cues");
		if full {
			cue_programs(&sheet, &indices, o.graphs);
		} else {
			cue_lines(&sheet, &indices);
		}
	}
	if let Some(waves) = waves {
		if show_cues {
			println!();
		}
		println!("waveforms");
		waves.iter().for_each(|l| println!("  {l}"));
	}
	Ok(())
}

fn cue_lines(sheet: &CueSheet, indices: &[usize]) {
	println!(
		"  {:>5} {:>6}  {:<32} {:>13}  {:<12} {:>6}  categories",
		"index", "id", "name", "length", "sequence", "layers"
	);
	let mut refused = 0;
	for &i in indices {
		match sheet.program(i) {
			Ok(p) => {
				let layers: usize = p.tracks.iter().map(|t| t.layers.len()).sum();
				let nested = p.tracks.iter().any(|t| !t.nested.is_empty());
				let actions = if p.action_tracks.is_empty() { "" } else { " +actions" };
				println!(
					"  {i:>5} {:>6}  {:<32} {:>13}  {:<12} {:>5}{}  {:?}{actions}",
					p.cue_id,
					p.name,
					seconds(p.length_ms),
					sequence_name(p.sequence_type),
					layers,
					if nested { "+" } else { " " },
					p.categories()
				);
			}
			Err(e) => {
				refused += 1;
				let name = sheet.cue_name(i).unwrap_or("");
				println!("  {i:>5} {:>6}  {name:<32} unresolved: {}", "?", e.root());
			}
		}
	}
	if refused > 0 {
		println!("  {refused} cue(s) did not resolve");
	}
}

fn sequence_name(t: SequenceType) -> String {
	match t {
		SequenceType::Polyphonic => "polyphonic".into(),
		SequenceType::Random => "random".into(),
		SequenceType::RandomNoRepeat => "random-norep".into(),
		SequenceType::ComboSequential => "combo".into(),
		other => format!("{other:?}"),
	}
}

fn cue_programs(sheet: &CueSheet, indices: &[usize], graphs: bool) {
	for (n, &index) in indices.iter().enumerate() {
		if n > 0 {
			println!();
		}
		let p = match sheet.program(index) {
			Ok(p) => p,
			Err(e) => {
				let name = sheet.cue_name(index).unwrap_or("");
				println!("  cue {name:?}, index {index}: unresolved: {}", e.root());
				continue;
			}
		};
		println!("  cue {:?}: id {}, index {}", p.name, p.cue_id, p.cue_index);
		println!("    length {}, layers span {:.1} ms", seconds(p.length_ms), p.span_ms());
		if let Some(s) = p.sequence_index {
			println!("    sequence[{s}], {}", sequence_name(p.sequence_type));
		}
		let scope = Scope { sheet, graphs };
		scope.params("    ", &p.params, &p.local_aisacs, &p.global_aisacs);
		scope.tracks("    ", &p.tracks);
		for a in &p.action_tracks {
			let target = match &a.target {
				ActionTarget::Cue { acb_name, cue_id } => format!("cue id {cue_id} of {acb_name:?}"),
				other => format!("{other:?}"),
			};
			println!("    action track[{}] on {target}", a.index);
			for e in &a.events {
				match e {
					ActionEvent::Start { at_ms } => println!("      +{at_ms:>6} ms  start"),
					ActionEvent::Stop { at_ms } => println!("      +{at_ms:>6} ms  stop"),
				}
			}
		}
	}
}

struct Scope<'s, 'a> {
	sheet: &'s CueSheet<'a>,
	graphs: bool,
}

impl Scope<'_, '_> {
	fn params(&self, pad: &str, params: &[CueParam], local: &[usize], global: &[String]) {
		for p in params {
			println!("{pad}{}", param(p));
		}
		for &a in local {
			match self.sheet.local_aisac(a) {
				Some(la) => {
					println!(
						"{pad}local AISAC[{}] type {}, control {}, auto-modulation {}, default {}",
						la.index, la.kind, la.control_id, la.auto_modulation_index, la.has_default
					);
					for g in la.graphs.iter().filter(|_| self.graphs) {
						println!("{pad}  graph[{}] type {}: {}", g.index, g.kind, points(&g.points));
					}
				}
				None => println!("{pad}local AISAC[{a}], not in AisacTable"),
			}
		}
		for g in global {
			println!("{pad}global AISAC {g:?}");
		}
	}

	fn tracks(&self, pad: &str, tracks: &[ProgramTrack]) {
		for t in tracks {
			let index = t.track_index.map_or("-".into(), |i| i.to_string());
			let weight = t.weight.map_or(String::new(), |w| format!(", weight {w}"));
			let end = t.end_ms.map_or(String::new(), |e| format!(", ends at {e} ms"));
			println!("{pad}track[{index}]{weight}{end}");
			let inner = format!("{pad}  ");
			self.params(&inner, &t.params, &t.local_aisacs, &t.global_aisacs);
			for l in &t.layers {
				let synth = l.synth_index.map_or(String::new(), |s| format!(" via synth[{s}]"));
				let source = match l.source {
					AwbSource::Internal => "embedded",
					AwbSource::External => "streaming",
				};
				let missing = if l.data.is_some() { "" } else { ", archive not loaded" };
				println!(
					"{inner}+{:>6} ms  waveform[{}]{synth}: {} {}ch {} Hz {:.1} ms, {source} #{}{missing}",
					l.start_ms,
					l.waveform,
					codec_name(l.encode_type),
					l.channels,
					l.sample_rate,
					l.duration_ms(),
					l.awb_id
				);
				self.params(&format!("{inner}    "), &l.params, &l.local_aisacs, &[]);
			}
			for n in &t.nested {
				self.nested(&inner, n);
			}
		}
	}

	fn nested(&self, pad: &str, n: &NestedSequence) {
		let synth = n.synth_index.map_or(String::new(), |s| format!(" via synth[{s}]"));
		println!(
			"{pad}+{:>6} ms  sequence[{}]{synth}, {}",
			n.start_ms,
			n.sequence_index,
			sequence_name(n.sequence_type)
		);
		let inner = format!("{pad}    ");
		self.params(&inner, &n.params, &n.local_aisacs, &n.global_aisacs);
		self.tracks(&inner, &n.tracks);
	}
}

fn codec_name(encode_type: i64) -> &'static str {
	match encode_type {
		0 => "ADX",
		2 => "HCA",
		3 => "HCA-MX",
		_ => "?",
	}
}

fn points(p: &[(f32, u16)]) -> String {
	let p: Vec<String> =
		p.iter().map(|(x, y)| format!("({x}, {})", f32::from(*y) / 10000.0)).collect();
	p.join(" ")
}

fn param(p: &CueParam) -> String {
	match p {
		CueParam::Volume(v) => format!("volume {v}"),
		CueParam::Pitch(c) => format!("pitch {c} cents"),
		CueParam::RandomPitch { center_cents, range_cents } => {
			format!("random pitch {center_cents} ± {range_cents} cents")
		}
		CueParam::MaxPitch(c) => format!("max pitch {c} cents"),
		CueParam::Categories(c) => format!("categories {c:?}"),
		CueParam::BusSendLevel { bus_name, level, .. } => format!("send {level} to bus {bus_name:?}"),
		CueParam::CueLimit { instances, slot, kind } => {
			let kind = kind.map_or(String::new(), |k| format!(", kind {k}"));
			format!("cue limit {instances} in slot {slot}{kind}")
		}
		CueParam::ComboSequential { combo_time_ms, loop_back } => {
			format!("combo within {combo_time_ms} ms, loops back to track {loop_back}")
		}
		CueParam::ReplayInterval { min_ms } => format!("replay interval {min_ms} ms"),
		CueParam::Biquad { kind, values } => format!("biquad kind {kind} {values:?}"),
		CueParam::Bandpass { low, high } => format!("band-pass {low}..{high}"),
		CueParam::Envelope { stage, value, curve } => {
			let curve = curve.map_or(String::new(), |c| format!(", curve {c:?}"));
			format!("envelope {stage:?} {value}{curve}")
		}
		CueParam::Selector { name } => format!("selector {name:?}"),
		CueParam::SelectorLabel { selector, label } => format!("only while {selector:?} is {label:?}"),
		CueParam::Other { table, code, payload } => {
			format!(
				"{table} command {code}, payload {}",
				if payload.is_empty() { "empty".into() } else { hex(payload) }
			)
		}
		other => format!("{other:?}"),
	}
}

// An ACB's waveforms, or those in `only`, beside their bitstreams' own headers
fn wave_lines(
	acb: &Acb,
	streaming: Option<&[u8]>,
	only: Option<&BTreeSet<usize>>,
) -> Result<Vec<String>> {
	let embedded = acb.internal_awb().transpose()?;
	let streaming = streaming.map(|b| Afs2::parse(b).map(|a| (a, b))).transpose()?;
	let mut out = vec![format!(
		"{:>5}  {:<6} {:>2} {:>6} {:>9}  {:<9} {:>5}  bitstream",
		"index", "codec", "ch", "rate", "length", "stored", "awb"
	)];
	for w in acb.waveforms() {
		if only.is_some_and(|o| !o.contains(&w.index)) {
			continue;
		}
		let archive = if w.streaming { streaming.as_ref() } else { embedded.as_ref() };
		let bitstream = match archive {
			Some((a, b)) => match a.entries.iter().find(|e| i64::from(e.id) == w.awb_id) {
				Some(e) => describe(&b[e.offset..e.offset + e.len]),
				None => "not in the archive".into(),
			},
			None => "archive not loaded".into(),
		};
		out.push(format!(
			"{:>5}  {:<6} {:>2} {:>6} {:>8.2}s  {:<9} {:>5}  {bitstream}",
			w.index,
			w.codec_name(),
			w.channels,
			w.sample_rate,
			w.duration_secs(),
			if w.streaming { "streaming" } else { "embedded" },
			w.awb_id
		));
	}
	Ok(out)
}

fn awb(bytes: &[u8]) -> Result<()> {
	let a = Afs2::parse(bytes)?;
	println!(
		"AWB: {} entries, alignment {}, subkey {}, {} bytes",
		a.entries.len(),
		a.align,
		a.subkey,
		bytes.len()
	);
	println!("\nwaveforms");
	println!("  {:>6} {:>10} {:>9}  bitstream", "id", "offset", "bytes");
	for e in &a.entries {
		println!(
			"  {:>6} {:>#10x} {:>9}  {}",
			e.id,
			e.offset,
			e.len,
			describe(&bytes[e.offset..e.offset + e.len])
		);
	}
	Ok(())
}

fn acf(acf: &Acf, graphs: bool) -> Result<()> {
	println!("ACF {:?}, version {:#010x} ({})", acf.name, acf.version, acf.version_string);

	println!("\ncategories");
	for c in &acf.categories {
		println!("  [{}] id {:<4} {:<24} group {}", c.index, c.id, c.name, c.group);
		for p in &c.params {
			println!("        {}", param(p));
		}
		for &g in &c.global_aisacs {
			let name = acf.global_aisacs.get(g).map_or("?", |a| a.name.as_str());
			println!("        global AISAC[{g}] {name:?}");
		}
	}

	for s in &acf.dsp_settings {
		let default = if s.name == acf.default_dsp_setting { ", the default" } else { "" };
		println!("\nDSP setting {:?}{default}", s.name);
		for b in &s.buses {
			println!("  bus {} {:<16} volume {}", b.number, b.name, b.volume);
			for e in &b.effects {
				let name = e.name().map_or_else(|| format!("{:?}", e.kind), str::to_string);
				let bypass = if e.bypass { ", bypassed" } else { "" };
				println!("    effect {name}{bypass}, {} parameter bytes", e.parameters.len());
				if e.matrix().is_none() && !e.parameters.is_empty() && e.parameters.len() % 4 == 0 {
					let floats: Vec<String> = e.floats().iter().map(f32::to_string).collect();
					println!("      parameters {}", floats.join(" "));
				}
				if let Some(m) = e.matrix() {
					for (i, row) in m.iter().enumerate() {
						if row.iter().any(|&g| g != 0.0) {
							let outs: Vec<String> = row
								.iter()
								.enumerate()
								.filter(|(_, g)| **g != 0.0)
								.map(|(o, g)| format!("{o}:{g}"))
								.collect();
							println!("      in {i} -> {}", outs.join(" "));
						}
					}
				}
			}
			for send in &b.sends {
				println!(
					"    send {} to {} {:?}, type {}",
					send.level, send.bus_number, send.bus_name, send.kind
				);
			}
		}
	}

	if !acf.aisac_controls.is_empty() {
		println!("\nAISAC controls");
		for c in &acf.aisac_controls {
			println!("  {:>5} {}", c.id, c.name);
		}
	}
	if !acf.global_aisacs.is_empty() {
		println!("\nglobal AISACs");
		for a in &acf.global_aisacs {
			let default = a.default_control.map_or(String::new(), |d| format!(", default {d}"));
			println!("  [{}] {:<28} control {}{default}", a.index, a.name, a.control_id);
			for g in &a.graphs {
				let target = graph_target(acf, g.kind);
				if graphs {
					println!("      graph[{}] type {}{target}: {}", g.index, g.kind, points(&g.points));
				} else {
					println!("      graph[{}] type {}{target}, {} point(s)", g.index, g.kind, g.points.len());
				}
			}
		}
	}
	if !acf.reacts.is_empty() {
		println!("\nREACT");
		let name = |i: usize| acf.categories.get(i).map_or("?", |c| c.name.as_str());
		for r in &acf.reacts {
			println!(
				"  {} ducks {} to {}, down {} ms, up {} ms, hold type {} for {} ms",
				name(r.source),
				name(r.dest),
				r.level,
				r.decrement_ms,
				r.increment_ms,
				r.hold_kind,
				r.hold_ms
			);
		}
	}
	if !acf.selectors.is_empty() {
		println!("\nselectors");
		for s in &acf.selectors {
			println!("  {}: {}", s.name, s.labels.join(", "));
		}
	}
	if !acf.voice_limit_groups.is_empty() {
		println!("\nvoice limit groups");
		for g in &acf.voice_limit_groups {
			println!("  {:<24} {}", g.name, g.max);
		}
	}
	Ok(())
}

// Graph types 7 and up drive the send to `aisac_graph_buses[type - 7]`.
fn graph_target(acf: &Acf, kind: i64) -> String {
	usize::try_from(kind - 7)
		.ok()
		.and_then(|k| acf.aisac_graph_buses.get(k))
		.map_or(String::new(), |b| format!(" (send to {b})"))
}

fn usm(u: &Usm, len: usize) -> Result<()> {
	println!("USM, {len} bytes");
	if let Some(v) = u.video {
		println!(
			"video {} {}x{} (shown {}x{}) at {:.3} fps, {} frames, alpha type {}",
			v.codec_name(),
			v.width,
			v.height,
			v.disp_width,
			v.disp_height,
			v.fps(),
			v.total_frames,
			v.alpha_type
		);
	}
	println!("\nstreams");
	for e in &u.dir {
		let what = if e.is_container() { "container".into() } else { stream_name(e.stmid) };
		println!(
			"  {what:<9} channel {:>2} {:<32} {:>10} bytes, {} bit/s",
			e.chno, e.filename, e.filesize, e.avbps
		);
	}
	println!("\nchunks");
	for (k, n) in u.chunk_counts() {
		println!("  {k:<8} {n:>7}");
	}
	Ok(())
}
