// A cue resolved into a playable program: which waveform starts when, with every authored parameter
// decoded. A parameter command this crate has no meaning for is kept as `CueParam::Other`, so a
// player that does not model every parameter of a program has to refuse it rather than play it
// differently. A sequencer command or a table feature that is not modeled fails the program.

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
	Category, // an ACF's `CommandTable`
}

impl std::fmt::Display for CommandTable {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(match self {
			Self::Sequence => "SeqCommandTable",
			Self::Track => "TrackCommandTable",
			Self::Synth => "SynthCommandTable",
			Self::TrackEvent => "TrackEventTable",
			Self::Category => "CommandTable",
		})
	}
}

fn unsupported(c: &Command, table: CommandTable) -> Error {
	Error::UnsupportedCommand { table, code: c.code, payload_len: c.payload.len() }
}

// A parameter command, scaled to its unit. Pitch stays in cents.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CueParam {
	Volume(f32), // code 87, player parameter 0
	Pitch(f32),  // code 5, player parameter 3
	// Codes 8 and 60, drawn per voice. Code 8 has a center of 0.
	RandomPitch { center_cents: f32, range_cents: f32 },
	// Code 121: the maximum pitch the voice reserves resampler headroom for. It never lowers the pitch.
	MaxPitch(f32),
	Categories(Vec<u32>), // code 65: ACF category IDs
	// Code 111: the send level to the ACF bus `StringValueTable[bus_name_index]`. Buses are found by
	// name, so an empty one reaches none.
	BusSendLevel { bus_name_index: u16, bus_name: String, level: f32 },
	// Code 79, or 73 on a category: at most `instances` plays share limit `slot` at once. At the
	// limit, a new play is refused when `kind` is 1; otherwise the oldest stops and gives the new one
	// its place. Code 73 carries no kind.
	CueLimit { instances: u16, slot: u16, kind: Option<u8> },
	// Code 94: a combo sequence's settings. A play within `combo_time_ms` of the last advances to the
	// next track, wrapping to `loop_back`; a later one starts again from track 0.
	ComboSequential { combo_time_ms: u32, loop_back: u16 },
	// Code 120: a play within `min_ms` of the cue's last is refused.
	ReplayInterval { min_ms: u16 },
	// Code 31: a biquad filter. `kind` 255 removes it; the meaning of the three values is unread.
	Biquad { kind: u8, values: [u16; 3] },
	// Code 32: the band-pass cutoffs, normalized. A negative `low` removes the filter.
	Bandpass { low: f32, high: f32 },
	// Codes 36 to 40, one stage each, as authored. Attack, decay and release may carry a curve's
	// type and strength.
	Envelope { stage: EnvelopeStage, value: u32, curve: Option<[u8; 2]> },
	// Code 99: the selector whose label picks among the sequence's tracks
	Selector { name: String },
	// Code 100: the track plays only while `selector` holds `label`
	SelectorLabel { selector: String, label: String },
	// Any other command, undecoded
	Other { table: CommandTable, code: u16, payload: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeStage {
	Attack,  // code 36
	Hold,    // code 37
	Decay,   // code 38
	Sustain, // code 39
	Release, // code 40
}

// `Sequence.Type`: which of a sequence's tracks a play starts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SequenceType {
	Polyphonic,      // 0, every track
	Random,          // 3, one, weighted by `ProgramTrack::weight`
	RandomNoRepeat,  // 4, one, never the last one
	ComboSequential, // 6, one in turn; see `CueParam::ComboSequential`
	Other(i64),
}

impl SequenceType {
	fn of(value: i64) -> Self {
		match value {
			0 => Self::Polyphonic,
			3 => Self::Random,
			4 => Self::RandomNoRepeat,
			6 => Self::ComboSequential,
			v => Self::Other(v),
		}
	}
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
	pub local_aisacs: Vec<usize>,   // the synth's `AisacTable` rows
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
	pub global_aisacs: Vec<String>,
	// The sequence's `TrackValues` entry for it: a random sequence's weight
	pub weight: Option<u16>,
	pub events: Vec<TrackEvent>,
	pub layers: Vec<Layer>,
	// The sequences its note-ons start, directly or through a synth
	pub nested: Vec<NestedSequence>,
	pub end_ms: Option<u32>,
}

// Another sequence a note-on starts, as a cue of its own would play it
#[derive(Debug, Clone)]
pub struct NestedSequence {
	pub start_ms: u32,
	pub synth_index: Option<usize>, // the synth whose reference reached it, whose parameters apply
	pub sequence_index: usize,
	pub sequence_type: SequenceType,
	pub params: Vec<CueParam>,
	pub local_aisacs: Vec<usize>,
	pub global_aisacs: Vec<String>,
	pub tracks: Vec<ProgramTrack>,
}

// A track that plays another cue rather than a waveform
#[derive(Debug, Clone)]
pub struct ActionTrack {
	pub index: usize, // into `ActionTrackTable`
	pub target: ActionTarget,
	pub events: Vec<ActionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionTarget {
	Cue { acb_name: String, cue_id: i64 }, // `TargetType` 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionEvent {
	Start { at_ms: u32 }, // code 7100
	Stop { at_ms: u32 },  // code 7101
}

#[derive(Debug, Clone)]
pub struct CueProgram {
	pub cue_index: usize,
	pub cue_id: i64,
	pub name: String,
	// `-1` when indeterminate; otherwise `max(layer.start_ms + layer.duration_ms)`, truncated
	pub length_ms: i64,
	pub sequence_index: Option<usize>,
	pub sequence_type: SequenceType,
	pub params: Vec<CueParam>, // the outermost of the three scopes
	pub local_aisacs: Vec<usize>,
	pub global_aisacs: Vec<String>,
	pub tracks: Vec<ProgramTrack>,
	pub action_tracks: Vec<ActionTrack>,
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

	// Every parameter at every scope, nested sequences included
	pub fn all_params(&self) -> Vec<&CueParam> {
		fn walk<'p>(tracks: &'p [ProgramTrack], out: &mut Vec<&'p CueParam>) {
			for t in tracks {
				out.extend(&t.params);
				out.extend(t.layers.iter().flat_map(|l| &l.params));
				for n in &t.nested {
					out.extend(&n.params);
					walk(&n.tracks, out);
				}
			}
		}
		let mut out: Vec<&CueParam> = self.params.iter().collect();
		walk(&self.tracks, &mut out);
		out
	}
}

// An ACB with its tables parsed once
pub struct CueSheet {
	acb: Acb,
	cue: Utf,
	sequence: Utf,
	track: Utf,
	track_event: Utf,
	action_track: Utf,
	synth: Utf,
	seq_cmd: Utf,
	track_cmd: Utf,
	synth_cmd: Utf,
	waveform: Utf,
	aisac: Utf,
	graph: Utf,
	global_aisac: Utf,
	strings: Vec<String>,
	names: HashMap<String, usize>,
	cue_names: Vec<String>,
	internal: Option<Afs2>, // over the ACB's own `AwbFile`
	external: Option<(Afs2, Vec<u8>)>,
}

impl CueSheet {
	pub fn open(acb: Acb) -> Result<Self> {
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
		let internal = acb.internal_awb().transpose()?.map(|(afs2, _)| afs2);
		Ok(Self {
			cue,
			sequence: acb.table_or_empty("SequenceTable"),
			track: acb.table_or_empty("TrackTable"),
			track_event: acb.table_or_empty("TrackEventTable"),
			action_track: acb.table_or_empty("ActionTrackTable"),
			synth: acb.table_or_empty("SynthTable"),
			seq_cmd: acb.table_or_empty("SeqCommandTable"),
			track_cmd: acb.table_or_empty("TrackCommandTable"),
			synth_cmd: acb.table_or_empty("SynthCommandTable"),
			waveform: acb.table_or_empty("WaveformTable"),
			aisac: acb.table_or_empty("AisacTable"),
			graph: acb.table_or_empty("GraphTable"),
			global_aisac: acb.table_or_empty("GlobalAisacReferenceTable"),
			strings,
			names,
			cue_names,
			internal,
			external: None,
			acb,
		})
	}

	// Resolves streaming waveforms to byte ranges too.
	pub fn with_external_awb(mut self, bytes: Vec<u8>) -> Result<Self> {
		self.external = Some((Afs2::parse(&bytes)?, bytes));
		Ok(self)
	}

	pub fn acb(&self) -> &Acb {
		&self.acb
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
			AwbSource::Internal => self.internal.as_ref().zip(self.acb.root.get_bytes(0, "AwbFile")),
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
			sequence_type: SequenceType::Polyphonic,
			params: Vec::new(),
			local_aisacs: Vec::new(),
			global_aisacs: Vec::new(),
			tracks: Vec::new(),
			action_tracks: Vec::new(),
		};

		match reference.kind {
			3 | 7 => {
				let seq = reference.index as usize;
				let s = self.sequence(seq, 0)?;
				program.sequence_index = Some(seq);
				program.sequence_type = s.sequence_type;
				program.params = s.params;
				program.local_aisacs = s.local_aisacs;
				program.global_aisacs = s.global_aisacs;
				program.tracks = s.tracks;
				program.action_tracks = self.action_tracks(seq)?;
			}
			// A cue pointing straight at a synth or waveform gets the one track the sequencer would
			// have run.
			1 | 2 | 6 => {
				let mut track = ProgramTrack {
					track_index: None,
					params: Vec::new(),
					local_aisacs: Vec::new(),
					global_aisacs: Vec::new(),
					weight: None,
					events: vec![TrackEvent::NoteOn { at_ms: 0, reference }],
					layers: Vec::new(),
					nested: Vec::new(),
					end_ms: None,
				};
				self.note_on(reference, 0, None, &mut track, 0)?;
				program.tracks.push(track);
			}
			k => return Err(Error::unsupported(Format::Acb, format!("cue reference type {k}"))),
		}
		Ok(program)
	}

	// A sequence's own scope and tracks, as a nested one; its start time and synth are the caller's.
	fn sequence(&self, seq: usize, depth: u32) -> Result<NestedSequence> {
		ensure_row("sequence", seq, &self.sequence)?;
		reject_unmodeled_scope("sequence", seq, &self.sequence)?;
		let sequence_type = SequenceType::of(self.sequence.get_i64(seq, "Type").unwrap_or(-1));
		let track_indexes = self.sequence.get_indexes(seq, "TrackIndex");
		let weights = self.sequence.get_indexes(seq, "TrackValues");
		if !weights.is_empty() && weights.len() != track_indexes.len() {
			let reason = format!(
				"sequence[{seq}]: {} TrackValues for {} tracks",
				weights.len(),
				track_indexes.len()
			);
			return Err(Error::malformed(Format::Acb, reason));
		}
		let mut tracks = Vec::with_capacity(track_indexes.len());
		for (i, &ti) in track_indexes.iter().enumerate() {
			let mut track = self.track(ti, depth)?;
			track.weight = weights.get(i).map(|&w| w as u16);
			tracks.push(track);
		}
		Ok(NestedSequence {
			start_ms: 0,
			synth_index: None,
			sequence_index: seq,
			sequence_type,
			params: self.params(CommandTable::Sequence, &self.sequence, seq, &self.seq_cmd)?,
			local_aisacs: self.sequence.get_indexes(seq, "LocalAisacs"),
			global_aisacs: self.global_aisacs(&self.sequence, seq)?,
			tracks,
		})
	}

	fn track(&self, ti: usize, depth: u32) -> Result<ProgramTrack> {
		ensure_row("track", ti, &self.track)?;
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
			global_aisacs: self.global_aisacs(&self.track, ti)?,
			weight: None,
			events: Vec::new(),
			layers: Vec::new(),
			nested: Vec::new(),
			end_ms: None,
		};

		let ei = self.track.get_i64(ti, "EventIndex").unwrap_or(-1);
		let Ok(ei) = usize::try_from(ei) else { return Ok(out) };
		let events = schedule(self.track_event.get_bytes(ei, "Command").unwrap_or_default())?;
		for e in &events {
			match *e {
				TrackEvent::NoteOn { at_ms, reference } => {
					self.note_on(reference, at_ms, None, &mut out, depth)?
				}
				TrackEvent::TrackEnd { at_ms } => out.end_ms = Some(at_ms),
			}
		}
		out.events = events;
		Ok(out)
	}

	// The `ActionTrackTable` rows of a cue's sequence. Their `CommandIndex` names a
	// `TrackEventTable` stream.
	fn action_tracks(&self, seq: usize) -> Result<Vec<ActionTrack>> {
		let count = self.sequence.get_i64(seq, "NumActionTracks").unwrap_or(0);
		let first = self.sequence.get_i64(seq, "ActionTrackStartIndex").unwrap_or(-1);
		let mut out = Vec::new();
		for a in (0..count.max(0)).map(|i| (first + i) as usize) {
			ensure_row("action track", a, &self.action_track)?;
			reject_unmodeled_scope("action track", a, &self.action_track)?;
			let t = &self.action_track;
			let target = match t.get_i64(a, "TargetType").unwrap_or(-1) {
				1 => ActionTarget::Cue {
					acb_name: t.get_str(a, "TargetAcbName").unwrap_or_default().to_string(),
					cue_id: t.get_i64(a, "TargetId").unwrap_or(-1),
				},
				k => {
					let feature = format!("action track[{a}]: target type {k}");
					return Err(Error::unsupported(Format::Acb, feature));
				}
			};
			if t.get_i64(a, "Scope").unwrap_or(0) != 0 {
				return Err(Error::unsupported(Format::Acb, format!("action track[{a}]: a scope")));
			}
			let aisacs = t.get_i64(a, "GlobalAisacNumRefs").unwrap_or(0) != 0
				|| t.get_bytes(a, "LocalAisacs").is_some_and(|b| !b.is_empty());
			if aisacs {
				return Err(Error::unsupported(Format::Acb, format!("action track[{a}]: AISACs")));
			}
			if let Ok(ei) = usize::try_from(t.get_i64(a, "EventIndex").unwrap_or(-1)) {
				let feature = format!("action track[{a}]: EventIndex {ei}");
				return Err(Error::unsupported(Format::Acb, feature));
			}
			let stream = usize::try_from(t.get_i64(a, "CommandIndex").unwrap_or(-1))
				.ok()
				.and_then(|ci| self.track_event.get_bytes(ci, "Command"))
				.unwrap_or_default();
			out.push(ActionTrack { index: a, target, events: action_schedule(stream)? });
		}
		Ok(out)
	}

	fn note_on(
		&self,
		r: Reference,
		at_ms: u32,
		synth: Option<usize>,
		track: &mut ProgramTrack,
		depth: u32,
	) -> Result<()> {
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
				track.layers.push(self.layer(w, at_ms, None, Vec::new(), Vec::new()));
				Ok(())
			}
			2 | 6 => {
				let s = r.index as usize;
				ensure_row("synth", s, &self.synth)?;
				let kind = self.synth.get_i64(s, "Type").unwrap_or(-1);
				if kind != 0 {
					let feature = format!("synth[{s}]: synth type {kind} (only 0, polyphonic)");
					return Err(Error::unsupported(Format::Acb, feature));
				}
				reject_unmodeled_scope("synth", s, &self.synth)?;
				if self.synth.get_bytes(s, "TrackValues").is_some_and(|b| !b.is_empty()) {
					return Err(Error::unsupported(Format::Acb, format!("synth[{s}]: TrackValues")));
				}
				if self.synth.get_i64(s, "GlobalAisacNumRefs").unwrap_or(0) != 0 {
					let feature = format!("synth[{s}]: global AISAC references");
					return Err(Error::unsupported(Format::Acb, feature));
				}
				let params = self.params(CommandTable::Synth, &self.synth, s, &self.synth_cmd)?;
				let aisacs = self.synth.get_indexes(s, "LocalAisacs");
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
						track.layers.push(self.layer(w, at_ms, Some(s), params.clone(), aisacs.clone()));
					} else {
						self.note_on(next, at_ms, Some(s), track, depth + 1)?;
					}
				}
				Ok(())
			}
			// Another sequence, played as its cue would be, under the synth that reached it
			3 | 7 => {
				let mut nested = self.sequence(r.index as usize, depth + 1)?;
				if self.sequence.get_i64(nested.sequence_index, "NumActionTracks").unwrap_or(0) != 0 {
					let feature = format!("{r}: action tracks in a nested sequence");
					return Err(Error::unsupported(Format::Acb, feature));
				}
				nested.start_ms = at_ms;
				nested.synth_index = synth;
				track.nested.push(nested);
				Ok(())
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
		local_aisacs: Vec<usize>,
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
			local_aisacs,
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

	// The names of a row's `GlobalAisacReferenceTable` entries
	fn global_aisacs(&self, owner: &Utf, row: usize) -> Result<Vec<String>> {
		let count = owner.get_i64(row, "GlobalAisacNumRefs").unwrap_or(0).max(0);
		let first = owner.get_i64(row, "GlobalAisacStartIndex").unwrap_or(-1);
		(0..count)
			.map(|i| {
				let r = (first + i) as usize;
				ensure_row("global AISAC reference", r, &self.global_aisac)?;
				Ok(self.global_aisac.get_str(r, "Name").unwrap_or_default().to_string())
			})
			.collect()
	}
}

fn ensure_row(what: &str, row: usize, t: &Utf) -> Result<()> {
	if row >= t.rows.len() {
		return Err(Error::malformed(Format::Acb, format!("{what}[{row}] out of range")));
	}
	Ok(())
}

// Columns that would change playback but are not modeled
fn reject_unmodeled_scope(what: &str, row: usize, t: &Utf) -> Result<()> {
	if t.get_i64(row, "ParameterPallet").unwrap_or(-1) != -1 {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: parameter pallets")));
	}
	// A sequence's action tracks are read on their own, from its cue.
	if what != "sequence" && t.get_i64(row, "NumActionTracks").unwrap_or(0) != 0 {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: action tracks")));
	}
	if what == "track" && t.get_bytes(row, "TrackValues").is_some_and(|b| !b.is_empty()) {
		return Err(Error::unsupported(Format::Acb, format!("{what}[{row}]: TrackValues")));
	}
	Ok(())
}

// A `TrackEventTable` stream's note-ons and track end at absolute times. 3999 does nothing.
pub fn schedule(stream: &[u8]) -> Result<Vec<TrackEvent>> {
	let table = CommandTable::TrackEvent;
	let mut cursor = Cursor::default();
	let mut out = Vec::new();
	for c in parse_commands(stream) {
		if cursor.advance(&c)? {
			continue;
		}
		match c.code {
			2000 => {
				let b = <&[u8; 4]>::try_from(&c.payload[..]).map_err(|_| unsupported(&c, table))?;
				out.push(TrackEvent::NoteOn { at_ms: cursor.ms(), reference: Reference::of(b) });
			}
			4000 => out.push(TrackEvent::TrackEnd { at_ms: cursor.ms() }),
			3999 => {}
			_ => return Err(unsupported(&c, table)),
		}
	}
	Ok(out)
}

// An action track's stream: starts and stops of its target, at absolute times
pub fn action_schedule(stream: &[u8]) -> Result<Vec<ActionEvent>> {
	let table = CommandTable::TrackEvent;
	let mut cursor = Cursor::default();
	let mut out = Vec::new();
	for c in parse_commands(stream) {
		if cursor.advance(&c)? {
			continue;
		}
		match c.code {
			7100 if c.payload.is_empty() => out.push(ActionEvent::Start { at_ms: cursor.ms() }),
			7101 if c.payload.is_empty() => out.push(ActionEvent::Stop { at_ms: cursor.ms() }),
			_ => return Err(unsupported(&c, table)),
		}
	}
	Ok(out)
}

// The sequencer's cursor: 999 adds to it, 2001 sets it.
#[derive(Default)]
struct Cursor(i64);

impl Cursor {
	fn advance(&mut self, c: &Command) -> Result<bool> {
		match c.code {
			999 => self.0 += be_u32(c, CommandTable::TrackEvent)? as i64,
			2001 => self.0 = be_u32(c, CommandTable::TrackEvent)? as i64,
			_ => return Ok(false),
		}
		Ok(true)
	}

	fn ms(&self) -> u32 {
		self.0.max(0) as u32
	}
}

// One parameter command. `strings` is the `StringValueTable` that bus, selector and label commands
// index.
pub(crate) fn decode_param(
	c: &Command,
	table: CommandTable,
	strings: &[String],
) -> Result<CueParam> {
	let pair = || -> Result<[u16; 2]> {
		let b = <&[u8; 4]>::try_from(&c.payload[..]).map_err(|_| unsupported(c, table))?;
		Ok([u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]])])
	};
	let string = |index: u16, allow_empty: bool| -> Result<String> {
		match strings.get(index as usize) {
			Some(s) if allow_empty || !s.is_empty() => Ok(s.clone()),
			_ => {
				let reason =
					format!("command {} names StringValueTable[{index}], which is empty or absent", c.code);
				Err(Error::malformed(Format::Acb, reason))
			}
		}
	};
	let p = &c.payload[..];
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
			if p.is_empty() || !p.len().is_multiple_of(4) {
				return Err(unsupported(c, table));
			}
			CueParam::Categories(p.as_chunks::<4>().0.iter().map(|b| u32::from_be_bytes(*b)).collect())
		}
		111 => {
			let [idx, level] = pair()?;
			CueParam::BusSendLevel {
				bus_name_index: idx,
				bus_name: string(idx, true)?,
				level: level as f32 * 1e-4,
			}
		}
		79 if p.len() == 5 => CueParam::CueLimit {
			instances: u16::from_be_bytes([p[0], p[1]]),
			slot: u16::from_be_bytes([p[2], p[3]]),
			kind: Some(p[4]),
		},
		73 if p.len() == 4 => CueParam::CueLimit {
			instances: u16::from_be_bytes([p[0], p[1]]),
			slot: u16::from_be_bytes([p[2], p[3]]),
			kind: None,
		},
		// The trailing 8 bytes are the time of the last play, which the runtime writes back.
		94 if p.len() == 14 => CueParam::ComboSequential {
			combo_time_ms: u32::from_be_bytes([p[0], p[1], p[2], p[3]]),
			loop_back: u16::from_be_bytes([p[4], p[5]]),
		},
		120 if p.len() == 10 => CueParam::ReplayInterval { min_ms: u16::from_be_bytes([p[0], p[1]]) },
		31 if p.len() == 7 => CueParam::Biquad {
			kind: p[0],
			values: [1, 3, 5].map(|i| u16::from_be_bytes([p[i], p[i + 1]])),
		},
		32 => {
			let [low, high] = pair()?;
			CueParam::Bandpass { low: low as i16 as f32 * 1e-4, high: high as i16 as f32 * 1e-4 }
		}
		36 | 38 | 40 if matches!(p.len(), 3 | 4) => CueParam::Envelope {
			stage: match c.code {
				36 => EnvelopeStage::Attack,
				38 => EnvelopeStage::Decay,
				_ => EnvelopeStage::Release,
			},
			value: u16::from_be_bytes([p[0], p[1]]) as u32,
			// A third byte alone is skipped.
			curve: (p.len() == 4).then(|| [p[2], p[3]]),
		},
		37 => CueParam::Envelope { stage: EnvelopeStage::Hold, value: be_u32(c, table)?, curve: None },
		39 => CueParam::Envelope {
			stage: EnvelopeStage::Sustain,
			value: be_u16(c, table)? as u32,
			curve: None,
		},
		99 => CueParam::Selector { name: string(be_u16(c, table)?, false)? },
		100 => {
			let [selector, label] = pair()?;
			CueParam::SelectorLabel { selector: string(selector, false)?, label: string(label, false)? }
		}
		73 | 79 | 94 | 120 | 31 | 36 | 38 | 40 => return Err(unsupported(c, table)),
		code => CueParam::Other { table, code, payload: c.payload.clone() },
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
	// `GraphTable.Type`. Which parameter each type drives is unread.
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

	fn strings() -> Vec<String> {
		["MasterOut", "BUS1", "BUS2", "BUS3", "Instrument", "Bell", ""]
			.iter()
			.map(|s| s.to_string())
			.collect()
	}

	fn one(raw: &str, table: CommandTable) -> CueParam {
		let c = parse_commands(&hex(raw));
		assert_eq!(c.len(), 1);
		decode_param(&c[0], table, &strings()).unwrap()
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

	// Start the target at once, stop it 50 ms later.
	#[test]
	fn action_events() {
		let ev = action_schedule(&hex("1bbc00 07d10400000032 1bbd00 0000")).unwrap();
		assert_eq!(ev, vec![ActionEvent::Start { at_ms: 0 }, ActionEvent::Stop { at_ms: 50 }]);
		// A note-on has no place in an action track.
		assert!(action_schedule(&hex("07d00400020003 0000")).is_err());
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
	fn playback_rules() {
		assert_eq!(
			one("004f05 0003000201 0000", CommandTable::Sequence),
			CueParam::CueLimit { instances: 3, slot: 2, kind: Some(1) }
		);
		assert_eq!(
			one("005e0e 00000050 0002 0000000000001234 0000", CommandTable::Sequence),
			CueParam::ComboSequential { combo_time_ms: 80, loop_back: 2 }
		);
		assert_eq!(
			one("00780a 0028 0000000000000000 0000", CommandTable::Sequence),
			CueParam::ReplayInterval { min_ms: 40 }
		);
		assert_eq!(
			one("006302 0004 0000", CommandTable::Sequence),
			CueParam::Selector { name: "Instrument".into() }
		);
		assert_eq!(
			one("006404 00040005 0000", CommandTable::Track),
			CueParam::SelectorLabel { selector: "Instrument".into(), label: "Bell".into() }
		);
		// The label is an empty string.
		let c = parse_commands(&hex("006404 00040006 0000"));
		assert!(decode_param(&c[0], CommandTable::Track, &strings()).is_err());
	}

	#[test]
	fn filters_and_envelope() {
		assert_eq!(
			one("001f07 02 1f40 0064 00c8 0000", CommandTable::Track),
			CueParam::Biquad { kind: 2, values: [8000, 100, 200] }
		);
		assert_eq!(
			one("002004 03e8 1388 0000", CommandTable::Track),
			CueParam::Bandpass { low: 1000.0 * 1e-4, high: 5000.0 * 1e-4 }
		);
		assert_eq!(
			one("002404 012c 0164 0000", CommandTable::Synth),
			CueParam::Envelope { stage: EnvelopeStage::Attack, value: 300, curve: Some([1, 100]) }
		);
		assert_eq!(
			one("002803 01f4 00 0000", CommandTable::Synth),
			CueParam::Envelope { stage: EnvelopeStage::Release, value: 500, curve: None }
		);
		assert_eq!(
			one("002504 00000028 0000", CommandTable::Synth),
			CueParam::Envelope { stage: EnvelopeStage::Hold, value: 40, curve: None }
		);
		assert_eq!(
			one("002702 07d0 0000", CommandTable::Synth),
			CueParam::Envelope { stage: EnvelopeStage::Sustain, value: 2000, curve: None }
		);
	}

	// A command with no decoded meaning is kept whole; one of a known code with the wrong size fails.
	#[test]
	fn other_commands() {
		assert_eq!(
			one("00210101 0000", CommandTable::Track),
			CueParam::Other { table: CommandTable::Track, code: 33, payload: vec![1] }
		);
		let c = parse_commands(&hex("004f04 00010002 0000"));
		let e = decode_param(&c[0], CommandTable::Sequence, &strings()).unwrap_err();
		let table = CommandTable::Sequence;
		assert!(matches!(
			e,
			Error::UnsupportedCommand { table: t, code: 79, payload_len: 4 } if t == table
		));
	}

	#[test]
	fn bus_send_past_strings() {
		let c = parse_commands(&hex("006f0400632710 0000"));
		assert!(decode_param(&c[0], CommandTable::Sequence, &strings()).is_err());
		assert_eq!(
			one("006f0400060000 0000", CommandTable::Sequence),
			CueParam::BusSendLevel { bus_name_index: 6, bus_name: String::new(), level: 0.0 }
		);
	}
}
