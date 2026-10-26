// ACB cue sheets: a one-row `@UTF` table whose cells hold further tables. A cue is reached through
// `CueName -> Cue -> Sequence -> Track -> TrackEvent`, whose note-ons lead on through `Synth` and
// `Waveform` to an AWB entry. `Acb::cues` gives a cue's reachable waveforms and its preview
// window; `program::CueSheet` decodes what playing it does.

pub mod program;

pub use program::{CueParam, CueSheet};

use crate::error::{Error, Format, Result, ensure};

use crate::afs2::Afs2;
use crate::utf::{Utf, Value};

// One command of either dialect
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
	pub code: u16,
	pub payload: Vec<u8>,
}

impl Command {
	pub fn value(&self) -> Option<u64> {
		match self.payload.len() {
			1 => Some(self.payload[0] as u64),
			2 => Some(u16::from_be_bytes(self.payload[..2].try_into().unwrap()) as u64),
			4 => Some(u32::from_be_bytes(self.payload[..4].try_into().unwrap()) as u64),
			_ => None,
		}
	}

	// Names for the codes this crate knows; any other prints as a number.
	pub fn name(&self) -> &'static str {
		match self.code {
			5 => "pitch cents",
			8 => "random pitch ±cents",
			60 => "random pitch center,±cents",
			65 => "set categories",
			87 => "volume ×0.01",
			110 => "unknown/110",
			111 => "bus send level",
			121 => "max pitch cents",
			998 => "seq time += random ms",
			999 => "seq time += ms",
			2000 => "note on",
			2001 => "seq time = ms",
			3999 => "no-op",
			4000 => "track end",
			_ => "",
		}
	}
}

// A command stream; trailing bytes after the terminator are ignored.
pub(crate) fn parse_commands(buf: &[u8]) -> Vec<Command> {
	let mut out = Vec::new();
	let mut p = 0;
	while p + 3 <= buf.len() {
		let code = u16::from_be_bytes(buf[p..p + 2].try_into().unwrap());
		let size = buf[p + 2] as usize;
		p += 3;
		if code == 0 && size == 0 {
			break;
		}
		let end = (p + size).min(buf.len());
		out.push(Command { code, payload: buf[p..end].to_vec() });
		p = end;
	}
	out
}

// A `(type, index)` pair into another table. 6 acts as 2 and 7 as 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
	pub kind: u16,
	pub index: u16,
}

impl Reference {
	pub(crate) fn of(b: &[u8; 4]) -> Self {
		Self { kind: u16::from_be_bytes([b[0], b[1]]), index: u16::from_be_bytes([b[2], b[3]]) }
	}
	pub(crate) fn of_cue(cue: &Utf, row: usize) -> Self {
		Self {
			kind: cue.get_i64(row, "ReferenceType").unwrap_or(0) as u16,
			index: cue.get_i64(row, "ReferenceIndex").unwrap_or(0) as u16,
		}
	}
}

impl std::fmt::Display for Reference {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self.kind {
			1 => write!(f, "waveform[{}]", self.index),
			2 => write!(f, "synth[{}]", self.index),
			3 => write!(f, "sequence[{}]", self.index),
			kind => write!(f, "type{kind}[{}]", self.index),
		}
	}
}

// A cue's playback window, as sequencer cursor positions in ms
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Window {
	pub start_ms: u64,          // at the first note-on
	pub end_ms: Option<u64>,    // at the track end; `None` is the end of the waveform
	pub track_end_marker: bool, // command 4000
}

#[derive(Debug, Clone)]
pub struct Cue {
	pub id: i64,
	pub name: String,
	pub reference: Reference,
	pub length_ms: i64, // `-1` when indeterminate
	pub window: Window,
	pub waveforms: Vec<usize>,
	pub commands: Vec<Command>, // in visit order
}

#[derive(Debug, Clone)]
pub struct Waveform {
	pub index: usize,
	pub encode_type: i64,
	pub streaming: bool,
	pub channels: i64,
	pub sample_rate: i64,
	pub num_samples: i64,
	pub loop_flag: i64,
	pub awb_id: i64,
}

impl Waveform {
	pub fn codec_name(&self) -> &'static str {
		match self.encode_type {
			0 => "ADX",
			1 => "?1",
			2 => "HCA",
			3 => "HCA-MX",
			4 => "VAG",
			5 => "ATRAC3",
			6 => "BCWAV",
			7 => "NINTENDO_DSP",
			_ => "?",
		}
	}

	pub fn duration_secs(&self) -> f64 {
		if self.sample_rate > 0 { self.num_samples as f64 / self.sample_rate as f64 } else { 0.0 }
	}
}

// `WaveformTable.SamplingRate`, a signed 16-bit field: 48000 arrives as -17536
pub(crate) fn sample_rate(waveforms: &Utf, row: usize) -> i64 {
	match waveforms.get_i64(row, "SamplingRate").unwrap_or(0) {
		v if v < 0 => v + 65536,
		v => v,
	}
}

pub struct Acb {
	pub name: String,
	root: Utf,
	buf: Vec<u8>,
}

impl Acb {
	pub fn open(path: &std::path::Path) -> Result<Self> {
		let buf =
			std::fs::read(path).map_err(|source| Error::Io { path: path.to_path_buf(), source })?;
		Self::from_bytes(buf)
	}

	pub fn from_bytes(buf: Vec<u8>) -> Result<Self> {
		let root = Utf::parse(&buf)?;
		ensure!(!root.rows.is_empty(), Error::malformed(Format::Acb, "the root table has no rows"));
		let name = root.get_str(0, "Name").unwrap_or_default().to_string();
		Ok(Self { name, root, buf })
	}

	pub fn root(&self) -> &Utf {
		&self.root
	}
	pub fn buffer(&self) -> &[u8] {
		&self.buf
	}

	// The root cells holding a nested table, with their row counts, empty ones included
	pub fn table_names(&self) -> Vec<(String, usize)> {
		let mut out = Vec::new();
		for c in &self.root.columns {
			if c.type_id != 0x0b {
				continue;
			}
			let rows = match self.root.get_table(0, &c.name) {
				Some(Ok(t)) => t.rows.len(),
				Some(Err(_)) => continue, // `AwbFile`, hashes
				None => 0,
			};
			out.push((c.name.clone(), rows));
		}
		out
	}

	pub fn table(&self, name: &str) -> Option<Result<Utf>> {
		self.root.get_table(0, name)
	}
	pub(crate) fn table_or_empty(&self, name: &str) -> Utf {
		match self.table(name) {
			Some(Ok(t)) => t,
			_ => Utf::empty(name),
		}
	}

	// The embedded AFS2, holding the memory-resident waveforms
	pub fn internal_awb(&self) -> Option<Result<(Afs2, &[u8])>> {
		let d = self.root.get_bytes(0, "AwbFile")?;
		(!d.is_empty()).then(|| Afs2::parse(d).map(|a| (a, d)))
	}

	pub fn waveforms(&self) -> Vec<Waveform> {
		let t = self.table_or_empty("WaveformTable");
		(0..t.rows.len())
			.map(|i| {
				let streaming = t.get_i64(i, "Streaming").unwrap_or(0) != 0;
				let awb_id = if streaming { "StreamAwbId" } else { "MemoryAwbId" };
				Waveform {
					index: i,
					encode_type: t.get_i64(i, "EncodeType").unwrap_or(-1),
					streaming,
					channels: t.get_i64(i, "NumChannels").unwrap_or(0),
					sample_rate: sample_rate(&t, i),
					num_samples: t.get_i64(i, "NumSamples").unwrap_or(0),
					loop_flag: t.get_i64(i, "LoopFlag").unwrap_or(0),
					awb_id: t.get_i64(i, awb_id).unwrap_or(-1),
				}
			})
			.collect()
	}

	pub fn cues(&self) -> Vec<Cue> {
		let cue_t = self.table_or_empty("CueTable");
		let name_t = self.table_or_empty("CueNameTable");
		let t = Tables {
			seq: self.table_or_empty("SequenceTable"),
			track: self.table_or_empty("TrackTable"),
			event: self.table_or_empty("TrackEventTable"),
			synth: self.table_or_empty("SynthTable"),
		};

		let mut names = vec![String::new(); cue_t.rows.len()];
		for r in 0..name_t.rows.len() {
			let idx = name_t.get_i64(r, "CueIndex").unwrap_or(-1);
			if let Ok(i) = usize::try_from(idx)
				&& i < names.len()
			{
				names[i] = name_t.get_str(r, "CueName").unwrap_or_default().to_string();
			}
		}

		(0..cue_t.rows.len())
			.map(|i| {
				let reference = Reference::of_cue(&cue_t, i);
				let mut cue = Cue {
					id: cue_t.get_i64(i, "CueId").unwrap_or(i as i64),
					name: names[i].clone(),
					reference,
					length_ms: cue_t.get_i64(i, "Length").unwrap_or(-1),
					// The cue's own sequence's: a `Preview` cue reaches a second sequence through a
					// synth, whose note-on at cursor 0 is not the preview's start.
					window: t.window(reference),
					waveforms: Vec::new(),
					commands: Vec::new(),
				};
				t.walk(reference, &mut cue, 0);
				cue.waveforms.sort_unstable();
				cue.waveforms.dedup();
				cue
			})
			.collect()
	}
}

// The tables `Acb::cues` walks
struct Tables {
	seq: Utf,
	track: Utf,
	event: Utf,
	synth: Utf,
}

impl Tables {
	// The command streams of a sequence's tracks
	fn streams(&self, seq: usize) -> impl Iterator<Item = &[u8]> {
		self.seq.get_indexes(seq, "TrackIndex").into_iter().filter_map(move |ti| {
			let ei = usize::try_from(self.track.get_i64(ti, "EventIndex").unwrap_or(-1)).ok()?;
			self.event.get_bytes(ei, "Command")
		})
	}

	// The depth cap guards against a cycle in authored data.
	fn walk(&self, r: Reference, cue: &mut Cue, depth: u32) {
		if depth > 8 {
			return;
		}
		let idx = r.index as usize;
		match r.kind {
			1 => cue.waveforms.push(idx),
			2 | 6 => {
				if let Some(Value::Data(items)) = self.synth.get(idx, "ReferenceItems") {
					for pair in items.as_chunks::<4>().0 {
						self.walk(Reference::of(pair), cue, depth + 1);
					}
				}
			}
			3 | 7 => {
				for stream in self.streams(idx) {
					for c in parse_commands(stream) {
						let next = match <&[u8; 4]>::try_from(&c.payload[..]) {
							Ok(b) if c.code == 2000 => Some(Reference::of(b)),
							_ => None,
						};
						cue.commands.push(c);
						if let Some(next) = next {
							self.walk(next, cue, depth + 1);
						}
					}
				}
			}
			_ => {}
		}
	}

	// The cursor over the tracks the cue's own reference names, not following note-ons. A cue that
	// does not point straight at a sequence has no window.
	fn window(&self, r: Reference) -> Window {
		let mut w = Window::default();
		if !matches!(r.kind, 3 | 7) {
			return w;
		}
		let mut started = false;
		for stream in self.streams(r.index as usize) {
			let mut cursor: u64 = 0;
			for c in parse_commands(stream) {
				match c.code {
					999 => cursor += c.value().unwrap_or(0),
					2001 => cursor = c.value().unwrap_or(0),
					2000 if !started => {
						started = true;
						w.start_ms = cursor;
					}
					4000 => {
						w.track_end_marker = true;
						w.end_ms = Some(cursor);
					}
					_ => {}
				}
			}
		}
		w
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_commands_by_size() {
		// A preview window's track event: wait, note-on, set the cursor, end.
		let raw = hex(
			"03e70400002ee0\
			 07d00400020003\
			 07d10400007530\
			 0fa000\
			 0000",
		);
		let c = parse_commands(&raw);
		assert_eq!(c.len(), 4);
		assert_eq!((c[0].code, c[0].value()), (999, Some(12000)));
		assert_eq!((c[1].code, c[1].value()), (2000, Some(0x0002_0003)));
		assert_eq!((c[2].code, c[2].value()), (2001, Some(30000)));
		assert_eq!((c[3].code, c[3].payload.len()), (4000, 0));
	}

	#[test]
	fn unterminated_stream() {
		// A size byte that runs past the buffer clamps.
		let c = parse_commands(&hex("07d0ff0001"));
		assert_eq!(c.len(), 1);
		assert_eq!(c[0].payload.len(), 2);
	}

	fn hex(s: &str) -> Vec<u8> {
		let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
		(0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
	}
}
