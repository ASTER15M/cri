// A cue resolved into a playable program: which waveform starts when, with every authored parameter
// decoded. A command whose effect is not modeled fails the program with
// `Error::UnsupportedCommand` rather than play differently.

use std::collections::HashMap;

use crate::error::{Error, Format, Result};

use super::{Acb, Command, Reference, parse_commands, sample_rate};
use crate::afs2::Afs2;
use crate::utf::{Utf, Value};

// The same code means different things in the two dialects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandTable {
	Sequence,
	Track,
	Synth,
	TrackEvent,
}

impl std::fmt::Display for CommandTable {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::Sequence => "SeqCommandTable",
			Self::Track => "TrackCommandTable",
			Self::Synth => "SynthCommandTable",
			Self::TrackEvent => "TrackEventTable",
		})
	}
}

fn unsupported(c: &Command, table: CommandTable) -> Error {
	Error::UnsupportedCommand { table, code: c.code, payload_len: c.payload.len() }
}

// A parameter command, scaled to its unit. Pitch stays in cents.
#[derive(Debug, Clone, PartialEq)]
pub enum CueParam {
	Volume(f32), // code 87, player parameter 0
	Pitch(f32),  // code 5, player parameter 3
	// Codes 8 and 60, drawn per voice. Code 8 has a center of 0.
	RandomPitch { center_cents: f32, range_cents: f32 },
	// Code 121: the maximum pitch the voice reserves resampler headroom for. It never lowers the pitch.
	MaxPitch(f32),
	Categories(Vec<u32>), // code 65: ACF category IDs
	// Code 111: the send level to the ACF bus `StringValueTable[bus_name_index]`
	BusSendLevel { bus_name_index: u16, bus_name: String, level: f32 },
}

// A sequencer command at an absolute time
#[derive(Debug, Clone, PartialEq)]
pub enum TrackEvent {
	NoteOn { at_ms: u32, reference: Reference }, // code 2000
	TrackEnd { at_ms: u32 },                     // code 4000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwbSource {
	Internal, // embedded in the ACB's `AwbFile` cell
	External, // the sibling `.awb`, given with `CueSheet::with_external_awb`
}

// One waveform a note-on starts
#[derive(Debug, Clone)]
pub struct Layer {
	pub start_ms: u32,
	pub synth_index: Option<usize>, // `None` when the note-on named the waveform directly
	pub params: Vec<CueParam>,      // applied under the track's
	pub waveform: usize,
	pub source: AwbSource,
	pub awb_id: i64,
	pub encode_type: i64,
	pub channels: i64,
	pub sample_rate: i64,
	pub num_samples: i64,
	// Not "this waveform loops": the codec header decides.
	pub loop_flag: i64,
	// The bitstream's range in `source`, when that archive is loaded
	pub data: Option<std::ops::Range<usize>>,
}

impl Layer {
	pub fn duration_ms(&self) -> f64 {
		if self.sample_rate > 0 {
			self.num_samples as f64 * 1000.0 / self.sample_rate as f64
		} else {
			0.0
		}
	}
}

#[derive(Debug, Clone)]
pub struct ProgramTrack {
	// `None` for the one track made up when a cue points straight at a synth or waveform
	pub track_index: Option<usize>,
	pub params: Vec<CueParam>,    // applied over the sequence's
	pub local_aisacs: Vec<usize>, // `AisacTable` rows, not applied; see `CueSheet::local_aisac`
	pub events: Vec<TrackEvent>,
	pub layers: Vec<Layer>,
	pub end_ms: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CueProgram {
	pub cue_index: usize,
	pub cue_id: i64,
	pub name: String,
	// `-1` when indeterminate; otherwise `max(layer.start_ms + layer.duration_ms)`, truncated
	pub length_ms: i64,
	pub sequence_index: Option<usize>,
	pub params: Vec<CueParam>, // the outermost of the three scopes
	pub tracks: Vec<ProgramTrack>,
}

impl CueProgram {
	// In command order
	pub fn categories(&self) -> Vec<u32> {
		let categories = self.params.iter().filter_map(|p| match p {
			CueParam::Categories(c) => Some(c.clone()),
			_ => None,
		});
		categories.flatten().collect()
	}

	pub fn span_ms(&self) -> f64 {
		let layers = self.tracks.iter().flat_map(|t| &t.layers);
		layers.map(|l| l.start_ms as f64 + l.duration_ms()).fold(0.0, f64::max)
	}
}

// An ACB with its tables parsed once
pub struct CueSheet<'a> {
	acb: &'a Acb,
	cue: Utf,
	sequence: Utf,
	track: Utf,
	track_event: Utf,
	synth: Utf,
	seq_cmd: Utf,
	track_cmd: Utf,
	synth_cmd: Utf,
	waveform: Utf,
	aisac: Utf,
	graph: Utf,
	strings: Vec<String>,
	names: HashMap<String, usize>,
	cue_names: Vec<String>,
	internal: Option<(Afs2, &'a [u8])>,
	external: Option<(Afs2, Vec<u8>)>,
}

impl<'a> CueSheet<'a> {
	pub fn open(acb: &'a Acb) -> Result<Self> {
		let cue = acb.table_or_empty("CueTable");
		let cue_name = acb.table_or_empty("CueNameTable");
		let mut names = HashMap::new();
		let mut cue_names = vec![String::new(); cue.rows.len()];
		for r in 0..cue_name.rows.len() {
			let idx = cue_name.get_i64(r, "CueIndex").unwrap_or(-1);
			if let Ok(i) = usize::try_from(idx)
				&& i < cue.rows.len()
			{
				let n = cue_name.get_str(r, "CueName").unwrap_or_default().to_string();
				names.insert(n.clone(), i);
				cue_names[i] = n;
			}
		}
		let string_table = acb.table_or_empty("StringValueTable");
		let strings = (0..string_table.rows.len())
			.map(|i| string_table.get_str(i, "StringValue").unwrap_or_default().to_string())
			.collect();
		let internal = acb.internal_awb().transpose()?;
		Ok(Self {
			acb,
			cue,
			sequence: acb.table_or_empty("SequenceTable"),
			track: acb.table_or_empty("TrackTable"),
			track_event: acb.table_or_empty("TrackEventTable"),
			synth: acb.table_or_empty("SynthTable"),
			seq_cmd: acb.table_or_empty("SeqCommandTable"),
			track_cmd: acb.table_or_empty("TrackCommandTable"),
			synth_cmd: acb.table_or_empty("SynthCommandTable"),
			waveform: acb.table_or_empty("WaveformTable"),
			aisac: acb.table_or_empty("AisacTable"),
			graph: acb.table_or_empty("GraphTable"),
			strings,
			names,
			cue_names,
			internal,
			external: None,
		})
	}

	// Resolves streaming waveforms to byte ranges too.
	pub fn with_external_awb(mut self, bytes: Vec<u8>) -> Result<Self> {
		self.external = Some((Afs2::parse(&bytes)?, bytes));
		Ok(self)
	}

	pub fn acb(&self) -> &Acb {
		self.acb
	}
	pub fn cue_count(&self) -> usize {
		self.cue.rows.len()
	}
	pub fn cue_index_by_name(&self, name: &str) -> Option<usize> {
		self.names.get(name).copied()
	}
	pub fn cue_index_by_id(&self, id: i64) -> Option<usize> {
		(0..self.cue.rows.len()).find(|&i| self.cue.get_i64(i, "CueId") == Some(id))
	}
	pub fn cue_name(&self, index: usize) -> Option<&str> {
		self.cue_names.get(index).map(String::as_str).filter(|s| !s.is_empty())
	}

	fn archive(&self, source: AwbSource) -> Option<(&Afs2, &[u8])> {
		match source {
			AwbSource::Internal => self.internal.as_ref().map(|(a, b)| (a, *b)),
			AwbSource::External => self.external.as_ref().map(|(a, b)| (a, b.as_slice())),
		}
	}

	// The ADX type-9 subkey of the layer's archive; 0 when it is not loaded
	pub fn layer_subkey(&self, layer: &Layer) -> u16 {
		self.archive(layer.source).map_or(0, |(a, _)| a.subkey)
	}
	pub fn layer_bytes(&self, layer: &Layer) -> Option<&[u8]> {
		let r = layer.data.clone()?;
		self.archive(layer.source).map(|(_, b)| &b[r])
	}

	// One `AisacTable` row. Type 1 follows the sequencer's own cursor.
	pub fn local_aisac(&self, index: usize) -> Option<LocalAisac> {
		if index >= self.aisac.rows.len() {
			return None;
		}
		let graphs = self.aisac.get_indexes(index, "GraphIndexes");
		Some(LocalAisac {
			index,
			kind: self.aisac.get_i64(index, "Type").unwrap_or(-1),
			control_id: self.aisac.get_i64(index, "ControlId").unwrap_or(-1),
			auto_modulation_index: self.aisac.get_i64(index, "AutoModulationIndex").unwrap_or(-1),
			has_default: self.aisac.get_i64(index, "DefaultControlFlag").unwrap_or(0) != 0,
			graphs: graphs.iter().map(|&g| self.graph_points(g)).collect(),
		})
	}

	fn graph_points(&self, index: usize) -> Graph {
		let f = |n: &str| self.graph.get_bytes(index, n).unwrap_or_default();
		let xs = f("Controls").as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c));
		let ys = f("Destinations").as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c));
		Graph {
			index,
			kind: self.graph.get_i64(index, "Type").unwrap_or(-1),
			points: xs.zip(ys).collect(),
		}
	}

	pub fn program(&self, cue_index: usize) -> Result<CueProgram> {
		let name = self.cue_name(cue_index).unwrap_or("").to_string();
		(self.program_inner(cue_index, &name)).map_err(|e| Error::Cue {
			index: cue_index,
			name,
			source: Box::new(e),
		})
	}
	pub fn program_by_name(&self, name: &str) -> Result<CueProgram> {
		let i = self.cue_index_by_name(name).ok_or_else(|| Error::NoSuchCue(name.to_string()))?;
		self.program(i)
	}
	pub fn program_by_id(&self, id: i64) -> Result<CueProgram> {
		let i = self.cue_index_by_id(id).ok_or_else(|| Error::NoSuchCue(format!("cue id {id}")))?;
		self.program(i)
	}

	fn program_inner(&self, cue_index: usize, name: &str) -> Result<CueProgram> {
		let reference = Reference::of_cue(&self.cue, cue_index);
		let mut program = CueProgram {
			cue_index,
			cue_id: self.cue.get_i64(cue_index, "CueId").unwrap_or(cue_index as i64),
			name: name.to_string(),
			length_ms: self.cue.get_i64(cue_index, "Length").unwrap_or(-1),
			sequence_index: None,
			params: Vec::new(),
			tracks: Vec::new(),
		};

		match reference.kind {
			3 | 7 => self.sequence_into(reference.index as usize, &mut program)?,
			// A cue pointing straight at a synth or waveform gets the one track the sequencer would
			// have run.
			1 | 2 | 6 => {
				let mut track = ProgramTrack {
					track_index: None,
					params: Vec::new(),
					local_aisacs: Vec::new(),
					events: vec![TrackEvent::NoteOn { at_ms: 0, reference }],
					layers: Vec::new(),
					end_ms: None,
				};
				self.note_on(reference, 0, &mut track, 0)?;
				program.tracks.push(track);
			}
			k => return Err(Error::unsupported(Format::Acb, format!("cue reference type {k}"))),
		}
		Ok(program)
	}

	fn sequence_into(&self, seq: usize, program: &mut CueProgram) -> Result<()> {
		program.sequence_index = Some(seq);
		let kind = self.sequence.get_i64(seq, "Type").unwrap_or(-1);
		// Type 0 plays every track. Random, switch and shuffle pick one: refused.
		if kind != 0 {
			let feature = format!("sequence[{seq}]: sequence type {kind} (only 0, polyphonic)");
			return Err(Error::unsupported(Format::Acb, feature));
		}
		reject_unmodeled_scope("sequence", seq, &self.sequence)?;
		program.params = self.params(CommandTable::Sequence, &self.sequence, seq, &self.seq_cmd)?;
		for ti in self.sequence.get_indexes(seq, "TrackIndex") {
			program.tracks.push(self.track(ti)?);
		}
		Ok(())
	}

	fn track(&self, ti: usize) -> Result<ProgramTrack> {
		let target = self.track.get_i64(ti, "TargetType").unwrap_or(0);
		if target != 0 {
			let feature = format!("track[{ti}]: target type {target}");
			return Err(Error::unsupported(Format::Acb, feature));
		}
		reject_unmodeled_scope("track", ti, &self.track)?;
		let mut out = ProgramTrack {
			track_index: Some(ti),
			params: self.params(CommandTable::Track, &self.track, ti, &self.track_cmd)?,
			local_aisacs: self.track.get_indexes(ti, "LocalAisacs"),
			events: Vec::new(),
			layers: Vec::new(),
			end_ms: None,
		};

		let ei = self.track.get_i64(ti, "EventIndex").unwrap_or(-1);
		let Ok(ei) = usize::try_from(ei) else { return Ok(out) };
		let events = schedule(self.track_event.get_bytes(ei, "Command").unwrap_or_default())?;
		for e in &events {
			match *e {
				TrackEvent::NoteOn { at_ms, reference } => self.note_on(reference, at_ms, &mut out, 0)?,
				TrackEvent::TrackEnd { at_ms } => out.end_ms = Some(at_ms),
			}
		}
		out.events = events;
		Ok(out)
	}

	fn note_on(&self, r: Reference, at_ms: u32, track: &mut ProgramTrack, depth: u32) -> Result<()> {
		if depth > 8 {
			let feature = format!("a reference chain deeper than 8 from {r}, possibly a cycle");
			return Err(Error::unsupported(Format::Acb, feature));
		}
		match r.kind {
			1 => {
				let w = r.index as usize;
				if w >= self.waveform.rows.len() {
					return Err(Error::malformed(Format::Acb, format!("waveform[{w}] out of range")));
				}
				track.layers.push(self.layer(w, at_ms, None, Vec::new()));
				Ok(())
			}
			2 | 6 => {
				let s = r.index as usize;
				let kind = self.synth.get_i64(s, "Type").unwrap_or(-1);
				if kind != 0 {
					let feature = format!("synth[{s}]: synth type {kind} (only 0, polyphonic)");
					return Err(Error::unsupported(Format::Acb, feature));
				}
				reject_unmodeled_scope("synth", s, &self.synth)?;
				let params = self.params(CommandTable::Synth, &self.synth, s, &self.synth_cmd)?;
				let Some(Value::Data(items)) = self.synth.get(s, "ReferenceItems") else {
					return Err(Error::malformed(Format::Acb, format!("synth[{s}]: no ReferenceItems")));
				};
				for pair in items.as_chunks::<4>().0 {
					let next = Reference::of(pair);
					if next.kind == 1 {
						let w = next.index as usize;
						if w >= self.waveform.rows.len() {
							let reason = format!("synth[{s}]: waveform[{w}] out of range");
							return Err(Error::malformed(Format::Acb, reason));
						}
						track.layers.push(self.layer(w, at_ms, Some(s), params.clone()));
					} else {
						self.note_on(next, at_ms, track, depth + 1)?;
					}
				}
				Ok(())
			}
			3 | 7 => {
				let feature = format!("a note-on into another cue's sequence[{}]", r.index);
				Err(Error::unsupported(Format::Acb, feature))
			}
			k => Err(Error::unsupported(Format::Acb, format!("a note-on into reference type {k}"))),
		}
	}

	fn layer(
		&self,
		w: usize,
		start_ms: u32,
		synth_index: Option<usize>,
		params: Vec<CueParam>,
	) -> Layer {
		let streaming = self.waveform.get_i64(w, "Streaming").unwrap_or(0) != 0;
		let source = if streaming { AwbSource::External } else { AwbSource::Internal };
		let awb_id =
			self.waveform.get_i64(w, if streaming { "StreamAwbId" } else { "MemoryAwbId" }).unwrap_or(-1);
		let data = (self.archive(source))
			.and_then(|(a, _)| a.entries.iter().find(|e| e.id as i64 == awb_id))
			.map(|e| e.offset..e.offset + e.len);
		Layer {
			start_ms,
			synth_index,
			params,
			waveform: w,
			source,
			awb_id,
			encode_type: self.waveform.get_i64(w, "EncodeType").unwrap_or(-1),
			channels: self.waveform.get_i64(w, "NumChannels").unwrap_or(0),
			sample_rate: sample_rate(&self.waveform, w),
			num_samples: self.waveform.get_i64(w, "NumSamples").unwrap_or(0),
			loop_flag: self.waveform.get_i64(w, "LoopFlag").unwrap_or(0),
			data,
		}
	}

	fn params(
		&self,
		which: CommandTable,
		owner: &Utf,
		row: usize,
		cmds: &Utf,
	) -> Result<Vec<CueParam>> {
		let Ok(ci) = usize::try_from(owner.get_i64(row, "CommandIndex").unwrap_or(-1)) else {
			return Ok(Vec::new());
		};
		let Some(stream) = cmds.get_bytes(ci, "Command") else { return Ok(Vec::new()) };
		parse_commands(stream).iter().map(|c| decode_param(c, which, &self.strings)).collect()
	}
}

// Columns that would change playback but are not modeled
fn reject_unmodeled_scope(what: &str, row: usize, t: &Utf) -> Result<()> {
	if t.get_i64(row, "ParameterPallet").unwrap_or(-1) != -1 {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: parameter pallets")));
	}
	if t.get_i64(row, "GlobalAisacNumRefs").unwrap_or(0) != 0 {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: global AISAC references")));
	}
	if t.get_i64(row, "NumActionTracks").unwrap_or(0) != 0 {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: action tracks")));
	}
	if t.get_bytes(row, "TrackValues").is_some_and(|b| !b.is_empty()) {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: TrackValues")));
	}
	Ok(())
}

// A `TrackEventTable` stream's note-ons and track end at absolute times. 3999 does nothing.
pub fn schedule(stream: &[u8]) -> Result<Vec<TrackEvent>> {
	let table = CommandTable::TrackEvent;
	let mut cursor: i64 = 0;
	let mut out = Vec::new();
	for c in parse_commands(stream) {
		match c.code {
			999 => cursor += be_u32(&c, table)? as i64,
			2001 => cursor = be_u32(&c, table)? as i64,
			2000 => {
				let b = <&[u8; 4]>::try_from(&c.payload[..]).map_err(|_| unsupported(&c, table))?;
				out.push(TrackEvent::NoteOn { at_ms: cursor.max(0) as u32, reference: Reference::of(b) });
			}
			4000 => out.push(TrackEvent::TrackEnd { at_ms: cursor.max(0) as u32 }),
			3999 => {}
			_ => return Err(unsupported(&c, table)),
		}
	}
	Ok(out)
}

// One parameter command. `bus_names` is the `StringValueTable` code 111 indexes.
fn decode_param(c: &Command, table: CommandTable, bus_names: &[String]) -> Result<CueParam> {
	let pair = || -> Result<[u16; 2]> {
		let b = <&[u8; 4]>::try_from(&c.payload[..]).map_err(|_| unsupported(c, table))?;
		Ok([u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]])])
	};
	Ok(match c.code {
		// Signed 16-bit
		5 => CueParam::Pitch(be_u16(c, table)? as i16 as f32),
		121 => CueParam::MaxPitch(be_u16(c, table)? as i16 as f32),
		87 => CueParam::Volume(be_u16(c, table)? as f32 * 0.01),
		8 => CueParam::RandomPitch { center_cents: 0.0, range_cents: be_u16(c, table)? as i16 as f32 },
		60 => {
			let [center, range] = pair()?;
			CueParam::RandomPitch { center_cents: center as i16 as f32, range_cents: range as i16 as f32 }
		}
		65 => {
			if c.payload.is_empty() || !c.payload.len().is_multiple_of(4) {
				return Err(unsupported(c, table));
			}
			CueParam::Categories(
				c.payload.as_chunks::<4>().0.iter().map(|p| u32::from_be_bytes(*p)).collect(),
			)
		}
		111 => {
			let [idx, level] = pair()?;
			let bus_name = bus_names.get(idx as usize).cloned().unwrap_or_default();
			if bus_name.is_empty() {
				let reason = format!("a bus send names StringValueTable[{idx}], which is empty or absent");
				return Err(Error::malformed(Format::Acb, reason));
			}
			CueParam::BusSendLevel { bus_name_index: idx, bus_name, level: level as f32 * 1e-4 }
		}
		_ => return Err(unsupported(c, table)),
	})
}

#[derive(Debug, Clone)]
pub struct LocalAisac {
	pub index: usize,
	pub kind: i64,
	pub control_id: i64,
	pub auto_modulation_index: i64,
	pub has_default: bool,
	pub graphs: Vec<Graph>,
}

#[derive(Debug, Clone)]
pub struct Graph {
	pub index: usize,
	// `GraphTable.Type`. What type 1, the only one seen so far, names is unknown.
	pub kind: i64,
	// `(Controls, Destinations)`, as authored; 10000 is unity. `Curve` is dropped: it is 100
	// everywhere.
	pub points: Vec<(f32, u16)>,
}

fn be_u32(c: &Command, table: CommandTable) -> Result<u32> {
	let b = <[u8; 4]>::try_from(&c.payload[..]).map_err(|_| unsupported(c, table))?;
	Ok(u32::from_be_bytes(b))
}

fn be_u16(c: &Command, table: CommandTable) -> Result<u16> {
	let b = <[u8; 2]>::try_from(&c.payload[..]).map_err(|_| unsupported(c, table))?;
	Ok(u16::from_be_bytes(b))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn hex(s: &str) -> Vec<u8> {
		let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
		(0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
	}

	fn buses() -> Vec<String> {
		["MasterOut", "BUS1", "BUS2", "BUS3"].iter().map(|s| s.to_string()).collect()
	}

	fn one(raw: &str, table: CommandTable) -> CueParam {
		let c = parse_commands(&hex(raw));
		assert_eq!(c.len(), 1);
		decode_param(&c[0], table, &buses()).unwrap()
	}

	// 2001 before the note-on sets its start time.
	#[test]
	fn leading_2001() {
		let ev = schedule(&hex("07d1040000012c 07d0040002000a 0000")).unwrap();
		assert_eq!(
			ev,
			vec![TrackEvent::NoteOn { at_ms: 300, reference: Reference { kind: 2, index: 10 } }]
		);
	}

	// A preview window: 999 moves the cursor to 12 s for the note-on, 2001 to 30 s for the end.
	#[test]
	fn trailing_2001() {
		let ev = schedule(&hex("03e70400002ee0 07d00400020003 07d10400007530 0fa000 0000")).unwrap();
		assert_eq!(
			ev,
			vec![
				TrackEvent::NoteOn { at_ms: 12000, reference: Reference { kind: 2, index: 3 } },
				TrackEvent::TrackEnd { at_ms: 30000 },
			]
		);
	}

	#[test]
	fn unmodeled_sequencer_command() {
		// 2003, a note-on with a per-note parameter block, is not modeled.
		let e = schedule(&hex("07d3060002000100000000")).unwrap_err();
		let table = CommandTable::TrackEvent;
		assert!(matches!(e, Error::UnsupportedCommand { table: t, code: 2003, .. } if t == table));
	}

	// Each parameter's unit
	#[test]
	fn parameter_scales() {
		assert_eq!(one("0057020019 0000", CommandTable::Sequence), CueParam::Volume(0.25));
		assert_eq!(one("00050200c8 0000", CommandTable::Track), CueParam::Pitch(200.0));
		// `ff38` is -200: pitch is signed.
		assert_eq!(one("000502ff38 0000", CommandTable::Track), CueParam::Pitch(-200.0));
		assert_eq!(one("0079020064 0000", CommandTable::Synth), CueParam::MaxPitch(100.0));
		assert_eq!(
			one("003c04ffce0050 0000", CommandTable::Track),
			CueParam::RandomPitch { center_cents: -50.0, range_cents: 80.0 }
		);
		assert_eq!(
			one("0008020078 0000", CommandTable::Track),
			CueParam::RandomPitch { center_cents: 0.0, range_cents: 120.0 }
		);
		assert_eq!(one("004104 00000007 0000", CommandTable::Sequence), CueParam::Categories(vec![7]));
		assert_eq!(
			one("006f0400011388 0000", CommandTable::Sequence),
			CueParam::BusSendLevel { bus_name_index: 1, bus_name: "BUS1".into(), level: 0.5 }
		);
	}

	#[test]
	fn unmodeled_parameter_command() {
		let c = parse_commands(&hex("00210101 0000"));
		let e = decode_param(&c[0], CommandTable::Track, &buses()).unwrap_err();
		let table = CommandTable::Track;
		assert!(matches!(
			e,
			Error::UnsupportedCommand { table: t, code: 33, payload_len: 1 } if t == table
		));
	}

	#[test]
	fn bus_send_past_strings() {
		let c = parse_commands(&hex("006f0400632710 0000"));
		assert!(decode_param(&c[0], CommandTable::Sequence, &buses()).is_err());
	}
}
