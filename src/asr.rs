// The Atom sound renderer's float output, reproduced offline: sound commands in, planes out, on the
// pass grid the native mixer renders on. Voices mix into the buses of an ACF's DSP setting, which
// run in graph order once per production block. A voice at a pitch or at another sample rate is
// resampled as the library resamples it. What this renders other than CRI would is counted in
// `Approximated`; a cue or a bus feature it cannot render at all fails the render.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::acb::program::{CueParam, CueProgram, CueSheet, Layer, SequenceType};
use crate::acf::{Acf, DspSetting};
use crate::error::{Error, Result};
use crate::{adx, hca};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsrConfig {
	pub rate: u32,
	pub server_frequency: u32,
	pub channels: usize,
	pub release_quantum_ticks: u32, // ticks of capacity the sink releases at once
	pub players: usize,             // created in order at startup, each seeding its generator
	pub seed: u32,                  // the global generator's
	pub keycode: Option<u64>,       // for encrypted ADX and HCA waveforms
}

impl AsrConfig {
	pub fn frames_per_tick(&self) -> u64 {
		u64::from(self.rate) / u64::from(self.server_frequency.max(1))
	}

	// `((105 * rate / server_frequency) / 100 + 31) & ~31`: 448 frames at 48 kHz and 120 Hz
	pub fn frames_per_pass(&self) -> u64 {
		((105 * self.frames_per_tick()) / 100 + 31) & !31
	}

	// How long a finished playback still holds its player past its last sample: 36 ms
	fn player_held_frames(&self) -> u64 {
		36 * u64::from(self.rate) / 1000
	}
}

// The core's largest block, and the length of a parameter crossfade
pub const PRODUCTION_CHUNK: u64 = 256;
const PRODUCTION_UNIT: u64 = 32;

// Added to frame 0 and subtracted from frame 1 of every plane at each live stage
pub const ASR_ANTI_DENORMAL: f32 = f32::from_bits(0x3580_0000);

// One command at the sound layer's boundary, on server tick `tick`
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
	pub tick: u64,
	pub player: usize,
	pub op: Op,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
	Play(Play),
	Stop,
	Pause(bool), // `false` releases a prepared play
	// An AISAC control the player keeps for its later plays, by its ACF `ControlId`
	Aisac { control_id: u32, value: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Play {
	pub sheet: usize, // into the mixer's sheets
	pub cue: Cue,
	pub start_time_ms: i64,
	pub prepare: bool, // started held, sounding from the `Pause(false)` that releases it
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Cue {
	Name(String),
	Id(i64),
}

// What the render does other than CRI would, counted so a residual can be attributed
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approximated {
	pub cues: BTreeSet<String>,
	pub non_stereo_layers: usize,
	// Effects run as a pass-through: anything but `Matrix`, and an `Echo` that was fed. Their tails
	// other than `Echo`'s are not modeled either.
	pub effects: BTreeSet<String>,
	// Global AISAC graphs whose target is not a bus send
	pub aisacs: BTreeSet<String>,
	// REACT rules whose source and destination categories both had a play: no ducking is applied.
	pub reacts: BTreeSet<String>,
}

impl Approximated {
	pub fn is_clean(&self) -> bool {
		*self == Self::default()
	}
}

pub struct Mix {
	pub pcm: Vec<f32>, // interleaved
	pub channels: usize,
	pub frames: u64,
	pub approximated: Approximated,
	// No voice limit is applied.
	pub playbacks: usize,
	pub peak_concurrent_layers: usize,
}

// The frame each audio pass begins. The rack refills the sink's queue to one pass's budget and
// stops at a refused submit, so the sink's release quantum decides the starts, which are not
// multiples of `frames_per_tick`. It depends on the configuration alone, so it is simulated as far
// as it is read.
#[derive(Debug)]
struct PassGrid {
	frames_per_tick: u64,
	rate: u64,
	budget: u64,
	released: u64, // capacity the sink releases each pass
	simulation: RefCell<GridSimulation>,
}

#[derive(Debug, Default)]
struct GridSimulation {
	starts: Vec<u64>,            // of every pass simulated
	production_starts: Vec<u64>, // each block of at most 256 frames; the graph runs once per block
	rendered: u64,               // where the next pass starts
	carried: u64,
	capacity: u64,
}

impl PassGrid {
	fn new(cfg: &AsrConfig) -> Self {
		Self {
			frames_per_tick: cfg.frames_per_tick(),
			rate: u64::from(cfg.rate),
			budget: cfg.frames_per_pass(),
			released: cfg.frames_per_tick() * u64::from(cfg.release_quantum_ticks.max(1)),
			simulation: RefCell::default(),
		}
	}

	// One more pass of the rack against the sink: it refills to one pass's budget in blocks of at
	// most `PRODUCTION_CHUNK`, rounded up to `PRODUCTION_UNIT`, and stops at the first submit not
	// taken whole. The overshoot carries into the next pass and shortens its budget.
	fn simulate_pass(&self, sim: &mut GridSimulation) {
		sim.starts.push(sim.rendered);
		sim.capacity += self.released;
		// The carried block goes first; it is at most one chunk, so a quantum takes it.
		let taken = sim.carried.min(sim.capacity);
		sim.capacity -= taken;
		let want = self.budget.saturating_sub(sim.carried);
		sim.carried -= taken;
		let mut produced = 0u64;
		while produced < want {
			let block = (want - produced).min(PRODUCTION_CHUNK).next_multiple_of(PRODUCTION_UNIT);
			sim.production_starts.push(sim.rendered + produced);
			produced += block;
			let accepted = block.min(sim.capacity);
			sim.capacity -= accepted;
			if accepted < block {
				sim.carried += block - accepted;
				break;
			}
		}
		sim.rendered += produced;
	}

	// Simulated through pass `pass` and past frame `frame`
	fn simulation(&self, pass: usize, frame: u64) -> std::cell::Ref<'_, GridSimulation> {
		{
			let mut sim = self.simulation.borrow_mut();
			while sim.starts.len() <= pass || sim.rendered <= frame {
				self.simulate_pass(&mut sim);
			}
		}
		self.simulation.borrow()
	}

	fn start(&self, pass: usize) -> u64 {
		self.simulation(pass, 0).starts[pass]
	}

	// Where production block `n` starts
	fn block(&self, n: usize) -> u64 {
		let mut sim = self.simulation.borrow_mut();
		while sim.production_starts.len() <= n {
			self.simulate_pass(&mut sim);
		}
		sim.production_starts[n]
	}

	// The production block starting at or after `frame`. Blocks follow each other with none longer
	// than a chunk, so one starts within a chunk of it.
	fn block_at_or_after(&self, frame: u64) -> u64 {
		let sim = self.simulation(0, frame + PRODUCTION_CHUNK);
		sim.production_starts[sim.production_starts.partition_point(|&b| b < frame)]
	}

	// How many frames a voice from `start` makes of `source` frames at `step`. The rack pulls each
	// voice once per block, and a resampling pull takes 8 source frames past what its output needs.
	// The pull that takes the last one ends the voice at its block's end, though the frames it holds
	// back could still make output.
	fn voice_frames(&self, start: u64, source: u64, step: u32) -> u64 {
		let whole = resampled_frames(source, step);
		if step == UNIT_STEP {
			return whole;
		}
		let lookahead = 8 * UNIT_STEP as u64;
		let last_pull = (source * UNIT_STEP as u64).saturating_sub(lookahead).div_ceil(step as u64);
		if last_pull >= whole {
			return whole;
		}
		whole.min(self.block_at_or_after(start + last_pull.max(1)) - start)
	}

	fn production_pulse(&self, frame: u64) -> f32 {
		let sim = self.simulation(0, frame);
		if sim.production_starts.binary_search(&frame).is_ok() {
			ASR_ANTI_DENORMAL
		} else if frame > 0 && sim.production_starts.binary_search(&(frame - 1)).is_ok() {
			-ASR_ANTI_DENORMAL
		} else {
			0.0
		}
	}

	// The Atom clock after `ticks` ticks, in whole microseconds
	fn clock_after(&self, ticks: usize) -> i64 {
		(ticks as u64 * self.frames_per_tick * 1_000_000 / self.rate.max(1)) as i64
	}

	// The clock the sequencer sees while pass `p` renders: the end of tick `p`
	fn cursor_us(&self, pass: usize) -> i64 {
		self.clock_after(pass + 1)
	}

	fn pass_of(&self, frame: u64) -> usize {
		let sim = self.simulation(0, frame);
		sim.starts.partition_point(|&s| s <= frame) - 1
	}
}

// Interleaved, at its own rate. A layer plays only stereo at the mixer's rate.
#[derive(Debug, Clone)]
pub struct Waveform {
	pub pcm: Vec<f32>,
	pub channels: usize,
	pub rate: u32,
}

// What a play is, before and after its prepare is released
#[derive(Clone)]
struct PlayState {
	sheet: usize,
	program: Arc<CueProgram>,
	pitches: Vec<Vec<Option<f32>>>, // in cents, per track, per layer
	start_time_ms: i64,
	controls: BTreeMap<u32, f32>, // the player's AISAC controls when it played
}

// One play, on the pass grid
struct Playback {
	play: PlayState,
	// Where the first sample comes out: the pass after the command's tick, its own tick's pass when
	// the player was still held, or when the command releases a prepared play
	onset_pass: usize,
	onset: u64,           // where `onset_pass` begins
	origin_us: i64,       // the clock when the cue started, which anchors its timeline
	release: Option<u64>, // where a stop began the release; silent from `release + PRODUCTION_CHUNK`
}

impl Playback {
	// Where a layer authored at `start_ms` begins, and how far into its waveform. A start time seeks
	// the cursor, which advances any layer it has passed.
	fn place(&self, grid: &PassGrid, start_ms: u32, rate: i64) -> (u64, usize) {
		let authored = start_ms as i64;
		let start_time_ms = self.play.start_time_ms;
		if authored <= start_time_ms {
			// A seek costs one more tick of cursor, truncated to whole ms; none at 0.
			let seek_advance = match start_time_ms > 0 {
				true => (grid.clock_after(self.onset_pass) - self.origin_us).div_euclid(1000),
				false => 0,
			};
			let ahead_ms = start_time_ms + seek_advance - authored;
			return (self.onset, (ahead_ms * rate / 1000).max(0) as usize);
		}
		let want = authored - start_time_ms;
		let mut pass = self.onset_pass;
		while (grid.cursor_us(pass) - self.origin_us).div_euclid(1000) < want {
			pass += 1;
		}
		(grid.start(pass).max(self.onset), 0)
	}

	// Past the last sample if nothing stops it
	fn natural_end(&self, grid: &PassGrid, rate: i64) -> u64 {
		let mut end = self.onset;
		for (t, track) in self.play.program.tracks.iter().enumerate() {
			for (l, layer) in track.layers.iter().enumerate() {
				let (start, skip) = self.place(grid, layer.start_ms, rate);
				let source = (layer.num_samples.max(0) as u64).saturating_sub(skip as u64);
				let step = step(layer.sample_rate as u32, rate as u32, self.play.pitches[t][l]);
				end = end.max(start + grid.voice_frames(start, source, step));
			}
		}
		end
	}

	fn stopping(&self, grid: &PassGrid, pass: usize) -> bool {
		self.release.is_some_and(|r| grid.start(pass) >= r)
	}
}

// One layer of one playback, as the rack's voice
struct Voice {
	playback: usize,
	waveform: Arc<Waveform>,
	step: u32, // source frames per output frame, in 1/4096ths
	start: u64,
	last: u64,
	skip: usize,
	gain: f32, // sequence × track × synth volume
	category_volume: f32,
	graphs: Vec<Vec<(f32, u16)>>, // its track's type-1 AISACs
	sends: Vec<(usize, f32)>,     // bus and level, AISACs applied
	// The crossfade in force: its pass, that pass's end, and the value it crosses from and to
	pass: usize,
	pass_end: u64,
	previous: f32,
	target: f32,
}

// A bus of the DSP setting, with what it carries between blocks
struct Bus {
	name: String,
	volume: f32,
	effects: Vec<BusEffect>,
	sends: Vec<(usize, f32)>, // destination and gain, formed once
	is_root: bool,
}

enum BusEffect {
	Matrix(Box<[[f32; 8]; 8]>),
	// The tail counter: frames since the last live input, and how long it still reports output
	Echo { since_input: u32, tail: u32 },
	PassThrough(String),
}

// The ACF's bus graph, processing order first: every bus after each bus that sends to it
struct BusGraph {
	buses: Vec<Bus>,
	order: Vec<usize>,
	root: usize,
}

impl BusGraph {
	fn of(setting: &DspSetting, rate: u32) -> Result<Self> {
		let index_of = |name: &str| setting.buses.iter().position(|b| b.name == name);
		let mut buses = Vec::with_capacity(setting.buses.len());
		for b in &setting.buses {
			let mut sends = Vec::new();
			for s in &b.sends {
				let dest = index_of(&s.bus_name).ok_or_else(|| {
					Error::unrenderable(format!(
						"bus {} sends to {}, which is not in {}",
						b.name, s.bus_name, setting.name
					))
				})?;
				// `BusLink.Type` 2 and 1 scale by the bus volume, 0 does not. A `Matrix` link mode
				// would multiply in per-channel gains; every link reaching here is diagonal.
				let gain = match s.kind {
					2 | 1 => s.level * b.volume,
					0 => s.level,
					k => return Err(Error::unrenderable(format!("bus {} has a link of type {k}", b.name))),
				};
				sends.push((dest, gain));
			}
			let effects = b
				.effects
				.iter()
				.filter(|e| !e.bypass)
				.map(|e| match (e.name(), e.matrix()) {
					(_, Some(m)) => BusEffect::Matrix(Box::new(m)),
					(Some("CRIWARE/Echo"), _) => {
						let p = e.floats();
						BusEffect::Echo {
							since_input: 0,
							tail: echo_tail(rate, *p.first().unwrap_or(&0.0), *p.get(1).unwrap_or(&0.0)),
						}
					}
					(name, _) => {
						BusEffect::PassThrough(name.map_or_else(|| format!("{:?}", e.kind), str::to_string))
					}
				})
				.collect();
			buses.push(Bus { name: b.name.clone(), volume: b.volume, effects, sends, is_root: false });
		}
		let root = (buses.iter().position(|b| b.sends.is_empty()))
			.ok_or_else(|| Error::unrenderable(format!("{} has no root bus", setting.name)))?;
		if buses.iter().filter(|b| b.sends.is_empty()).count() > 1 {
			return Err(Error::unrenderable(format!(
				"{} has more than one bus that sends nowhere",
				setting.name
			)));
		}
		buses[root].is_root = true;

		// Kahn's order, lowest bus first among the ready ones
		let mut pending: Vec<usize> = vec![0; buses.len()];
		for b in &buses {
			for &(d, _) in &b.sends {
				pending[d] += 1;
			}
		}
		let mut order = Vec::with_capacity(buses.len());
		let mut ready: BTreeSet<usize> = (0..buses.len()).filter(|&i| pending[i] == 0).collect();
		while let Some(i) = ready.pop_first() {
			order.push(i);
			for &(d, _) in &buses[i].sends {
				pending[d] -= 1;
				if pending[d] == 0 {
					ready.insert(d);
				}
			}
		}
		if order.len() != buses.len() {
			return Err(Error::unrenderable(format!("{}'s buses send in a cycle", setting.name)));
		}
		Ok(Self { buses, order, root })
	}
}

// How long an echo keeps reporting output after its last live input, in frames: until its feedback
// has decayed below 3.0554878e-5 (`0x3800280D`), as the library computes it in `f32`
fn echo_tail(rate: u32, delay_ms: f32, feedback: f32) -> u32 {
	let delay_ms = if delay_ms < 1000.0 { delay_ms.max(1.0) } else { 1000.0 };
	let delay = ((rate as f32 * delay_ms) * 0.001) as i32;
	let feedback = if feedback < 0.99 { feedback.max(-0.99) } else { 0.99 };
	let tail = delay as f32 * ((f32::from_bits(0x3800280D).ln() / feedback.ln()) + 1.0);
	// A NaN converts as the x86 conversion does, to `i32::MIN`.
	if tail.is_nan() { 0x8000_0000 } else { tail as i32 as u32 }
}

// The DSP setting `name`'s position in the ACF
fn dsp_setting(acf: &Acf, name: &str) -> Result<usize> {
	(acf.dsp_settings.iter().position(|s| s.name == name))
		.ok_or_else(|| Error::unrenderable(format!("the ACF has no DSP setting {name}")))
}

// Holds the decoded waveforms: most plays repeat a cue.
pub struct AsrMixer {
	sheets: Vec<Arc<CueSheet>>,
	acf: Arc<Acf>,
	setting: usize, // in `acf.dsp_settings`
	cfg: AsrConfig,
	waveforms: HashMap<(usize, usize), Arc<Waveform>>,
	programs: HashMap<(usize, Cue), Arc<CueProgram>>,
	own_cues: usize, // given by `add_cue`, each a sheet after the ACBs'
}

impl AsrMixer {
	// Through the ACF's default DSP setting
	pub fn new(sheets: Vec<Arc<CueSheet>>, acf: Arc<Acf>, cfg: AsrConfig) -> Result<Self> {
		let setting = dsp_setting(&acf, &acf.default_dsp_setting)?;
		let (waveforms, programs) = (HashMap::new(), HashMap::new());
		Ok(Self { sheets, acf, setting, cfg, waveforms, programs, own_cues: 0 })
	}

	// A cue of the caller's own, not in any ACB: `program`, its layers playing `waveforms` by
	// `Layer::waveform`, whose lengths it takes. It plays as sheet `n` of a mixer whose ACB sheets
	// and earlier own cues number `n`, by its ID or name, and carries nothing that needs a sheet.
	pub fn add_cue(&mut self, mut program: CueProgram, waveforms: Vec<Waveform>) -> Result<usize> {
		renderable(&program)?;
		let refuse = |what: String| Err(Error::unrenderable(format!("cue {}: {what}", program.name)));
		if program.tracks.iter().any(|t| !t.local_aisacs.is_empty()) {
			return refuse("track AISACs, which need a sheet".into());
		}
		let layers = program.tracks.iter().flat_map(|t| &t.layers);
		if let Some(l) = layers.clone().find(|l| l.waveform >= waveforms.len()) {
			return refuse(format!("waveform[{}] of {}", l.waveform, waveforms.len()));
		}
		for layer in program.tracks.iter_mut().flat_map(|t| &mut t.layers) {
			let w = &waveforms[layer.waveform];
			layer.channels = w.channels as i64;
			layer.sample_rate = i64::from(w.rate);
			layer.num_samples = (w.pcm.len() / w.channels.max(1)) as i64;
		}
		let sheet = self.sheets.len() + self.own_cues;
		self.own_cues += 1;
		for (i, w) in waveforms.into_iter().enumerate() {
			self.waveforms.insert((sheet, i), Arc::new(w));
		}
		let program = Arc::new(program);
		self.programs.insert((sheet, Cue::Id(program.cue_id)), program.clone());
		self.programs.insert((sheet, Cue::Name(program.name.clone())), program);
		Ok(sheet)
	}

	pub fn with_dsp_setting(mut self, name: &str) -> Result<Self> {
		self.setting = dsp_setting(&self.acf, name)?;
		Ok(self)
	}

	pub fn config(&self) -> AsrConfig {
		self.cfg
	}

	// Replays `commands`, sorted by tick, over `ticks` ticks.
	pub fn render(&mut self, commands: &[Command], ticks: u64) -> Result<Mix> {
		let caches = (std::mem::take(&mut self.waveforms), std::mem::take(&mut self.programs));
		let mixer = AsrMixer {
			sheets: self.sheets.clone(),
			acf: self.acf.clone(),
			waveforms: caches.0,
			programs: caches.1,
			..*self
		};
		let mut stream = mixer.stream()?;
		let pushed = commands.iter().try_for_each(|c| stream.push(c));
		let frames = ticks * self.cfg.frames_per_tick();
		let mut pcm = vec![0.0f32; frames as usize * self.cfg.channels];
		if pushed.is_ok() {
			stream.fill(&mut pcm);
		}
		let mix = Mix {
			pcm,
			channels: self.cfg.channels,
			frames,
			approximated: stream.approximated.clone(),
			playbacks: stream.playbacks.len(),
			peak_concurrent_layers: stream.peak_concurrent_layers(frames),
		};
		let mixer = stream.into_mixer();
		(self.waveforms, self.programs) = (mixer.waveforms, mixer.programs);
		pushed.map(|()| mix)
	}

	// The mixer as a stream, from tick 0
	pub fn stream(self) -> Result<AsrStream> {
		let graph = BusGraph::of(&self.acf.dsp_settings[self.setting], self.cfg.rate)?;
		let mut global = seeded(self.cfg.seed);
		let slots = (0..self.cfg.players)
			.map(|_| {
				let rng = seeded(next(&mut global) % 0x8000_0000);
				next(&mut global); // the second generator, which no pitch draws from
				Slot { controls: BTreeMap::new(), active: None, held: None, occupied_until: None, rng }
			})
			.collect();
		let planes = vec![vec![0.0; PRODUCTION_CHUNK as usize * self.cfg.channels]; graph.buses.len()];
		Ok(AsrStream {
			grid: PassGrid::new(&self.cfg),
			live: vec![false; graph.buses.len()],
			planes,
			graph,
			slots,
			playbacks: Vec::new(),
			layers: Vec::new(),
			voices: Vec::new(),
			waiting: BTreeSet::new(),
			sounding: BTreeSet::new(),
			spans: Vec::new(),
			block: 0,
			rendered: Vec::new(),
			handed: 0,
			frames: 0,
			last_tick: 0,
			played: BTreeSet::new(),
			approximated: Approximated::default(),
			mixer: self,
		})
	}

	fn program(&mut self, sheet: usize, cue: &Cue) -> Result<Arc<CueProgram>> {
		let key = (sheet, cue.clone());
		if let Some(p) = self.programs.get(&key) {
			return Ok(p.clone());
		}
		let s = (self.sheets.get(sheet))
			.ok_or_else(|| Error::unrenderable(format!("no cue sheet {sheet}")))?;
		let program = match cue {
			Cue::Id(id) => s.program_by_id(*id),
			Cue::Name(name) => s.program_by_name(name),
		}?;
		renderable(&program)?;
		let program = Arc::new(program);
		self.programs.insert(key, program.clone());
		Ok(program)
	}

	fn waveform(&mut self, sheet: usize, layer: &Layer) -> Result<Arc<Waveform>> {
		if let Some(w) = self.waveforms.get(&(sheet, layer.waveform)) {
			return Ok(w.clone());
		}
		let s = (self.sheets.get(sheet))
			.ok_or_else(|| Error::unrenderable(format!("no cue sheet {sheet}")))?;
		let bytes = (s.layer_bytes(layer))
			.ok_or_else(|| Error::unrenderable(format!("waveform[{}] is not loaded", layer.waveform)))?;
		let subkey = s.layer_subkey(layer);
		let (pcm, channels, rate) = match layer.encode_type {
			0 => {
				let (header, pcm) = adx::decode_stream(bytes, self.cfg.keycode, subkey)?;
				(pcm, header.channels as usize, header.sample_rate)
			}
			2 => {
				let (header, pcm) = hca::decode_stream(bytes, self.cfg.keycode, subkey, None)?;
				(pcm, header.channels as usize, header.sample_rate)
			}
			other => {
				let reason =
					format!("waveform[{}] is encode type {other}, neither ADX nor HCA", layer.waveform);
				return Err(Error::unrenderable(reason));
			}
		};
		let w = Arc::new(Waveform { pcm, channels, rate });
		self.waveforms.insert((sheet, layer.waveform), w.clone());
		Ok(w)
	}

	// A playback's layers as voices, in track and layer order
	#[allow(clippy::too_many_arguments)]
	fn voices(
		&mut self,
		index: usize,
		playback: &Playback,
		grid: &PassGrid,
		graph: &BusGraph,
		out: &mut Vec<Voice>,
		approximated: &mut Approximated,
	) -> Result<()> {
		let program = playback.play.program.clone();
		let sheet = playback.play.sheet;
		let sequence_volume = volume_of(&program.params).unwrap_or(1.0);
		let rate = self.cfg.rate as i64;
		let (category_volume, aisacs) = self.categories(&program)?;

		for (t, track) in program.tracks.iter().enumerate() {
			let track_volume = volume_of(&track.params).unwrap_or(1.0);
			// The track's type-1 AISACs, on the sequencer's ms cursor, once per pass
			let mut graphs: Vec<Vec<(f32, u16)>> = Vec::new();
			for aisac in track.local_aisacs.iter().filter_map(|&i| self.sheets[sheet].local_aisac(i)) {
				for g in aisac.graphs {
					if g.kind != 1 {
						let reason = format!(
							"cue {}: local AISAC {} graph {} is type {}",
							program.name, aisac.index, g.index, g.kind
						);
						return Err(Error::unrenderable(reason));
					}
					graphs.push(g.points);
				}
			}

			for (l, layer) in track.layers.iter().enumerate() {
				let synth_volume = volume_of(&layer.params).unwrap_or(1.0);
				let gain = sequence_volume * track_volume * synth_volume;

				let waveform = self.waveform(sheet, layer)?;
				let step = step(waveform.rate, self.cfg.rate, playback.play.pitches[t][l]);
				if waveform.channels != 2 {
					approximated.non_stereo_layers += 1;
					approximated.cues.insert(format!("{} ({} ch)", program.name, waveform.channels));
					continue;
				}

				// A note-on at `T` starts in the first pass whose cursor, microseconds since the cue
				// started truncated to ms, reaches `T`.
				let (start, skip) = playback.place(grid, layer.start_ms, rate);
				let source_frames = waveform.pcm.len() / 2;
				if skip >= source_frames {
					continue;
				}
				let available = grid.voice_frames(start, (source_frames - skip) as u64, step);
				// A note-on due at or after the pass a stop lands in never happens.
				if playback.release.is_some_and(|r| start >= r) {
					continue;
				}
				let end = playback.release.map_or(u64::MAX, |r| r + PRODUCTION_CHUNK);
				if start >= end {
					continue;
				}

				let sends = self.sends(
					&program,
					track,
					layer,
					&playback.play.controls,
					&aisacs,
					graph,
					approximated,
				)?;
				let pass = grid.pass_of(start);
				// A voice new in this pass has no previous value, so its first pass steps.
				let first = automation(playback, grid, &graphs, pass);
				out.push(Voice {
					playback: index,
					waveform,
					step,
					start,
					last: (start + available).min(end),
					skip,
					gain,
					category_volume,
					graphs: graphs.clone(),
					sends,
					pass,
					pass_end: grid.start(pass + 1),
					previous: first,
					target: first,
				});
			}
		}
		Ok(())
	}

	// The product of the cue's category volumes, and the global AISACs its categories list, in
	// order
	fn categories(&self, program: &CueProgram) -> Result<(f32, Vec<usize>)> {
		let mut volume = 1.0;
		let mut aisacs = Vec::new();
		for id in program.categories() {
			let c = (self.acf.category_by_id(id)).ok_or_else(|| {
				Error::unrenderable(format!("cue {}: category {id} is not in the ACF", program.name))
			})?;
			if let Some(v) = c.volume() {
				volume *= v;
			}
			if c.params.iter().any(|p| !matches!(p, CueParam::Volume(_))) {
				let reason = format!("cue {}: category {}'s {:?}", program.name, c.name, c.params);
				return Err(Error::unrenderable(reason));
			}
			aisacs.extend(&c.global_aisacs);
		}
		Ok((volume, aisacs))
	}

	// A layer's bus sends: the product of every scope's level per bus, then each category AISAC
	// that targets a bus send scales it, or creates it at the graph's value. An AISAC whose control
	// was never written, and has no default, does nothing.
	#[allow(clippy::too_many_arguments)]
	fn sends(
		&self,
		program: &CueProgram,
		track: &crate::acb::program::ProgramTrack,
		layer: &Layer,
		controls: &BTreeMap<u32, f32>,
		aisacs: &[usize],
		graph: &BusGraph,
		approximated: &mut Approximated,
	) -> Result<Vec<(usize, f32)>> {
		let mut levels: Vec<(String, f32)> = Vec::new();
		let mut set =
			|name: &str, f: &dyn Fn(Option<f32>) -> f32| match levels.iter_mut().find(|(n, _)| n == name)
			{
				Some((_, l)) => *l = f(Some(*l)),
				None => levels.push((name.to_string(), f(None))),
			};
		for scope in [&program.params, &track.params, &layer.params] {
			for p in scope.iter() {
				if let CueParam::BusSendLevel { bus_name, level, .. } = p {
					set(bus_name, &|l| l.map_or(*level, |l| l * *level));
				}
			}
		}
		for &a in aisacs {
			let aisac = (self.acf.global_aisacs.get(a)).ok_or_else(|| {
				Error::unrenderable(format!("cue {}: global AISAC {a} is not in the ACF", program.name))
			})?;
			let Some(control) = controls.get(&aisac.control_id).copied().or(aisac.default_control) else {
				continue;
			};
			for g in &aisac.graphs {
				match (g.kind - 7).try_into().ok().and_then(|k: usize| self.acf.aisac_graph_buses.get(k)) {
					Some(bus) if (7..=14).contains(&g.kind) => {
						let value = global_graph_at(&g.points, control);
						set(bus, &|l| l.map_or(value, |l| value * l));
					}
					_ => {
						approximated.aisacs.insert(format!("{} (type {})", aisac.name, g.kind));
					}
				}
			}
		}
		let mut out = Vec::new();
		for (name, level) in levels {
			if name.is_empty() || level == 0.0 {
				continue;
			}
			let bus = (graph.buses.iter().position(|b| b.name == name)).ok_or_else(|| {
				Error::unrenderable(format!(
					"cue {} sends to bus {name}, which is not in the DSP setting",
					program.name
				))
			})?;
			out.push((bus, level));
		}
		Ok(out)
	}
}

// A player, as the stream keeps it. It keeps its AISAC controls, as nothing clears them.
struct Slot {
	controls: BTreeMap<u32, f32>,
	active: Option<usize>,
	held: Option<PlayState>,
	// The frame the player is free from. A play while it still holds a playback sounds a pass early,
	// overlapping the outgoing one's release.
	occupied_until: Option<u64>,
	rng: [u32; 4],
}

// The mixer as a stream: commands go in no earlier than the pass that renders next, and frames come
// out as they are asked for. No command reaches back before its own tick's pass, so a voice is laid
// out when its play arrives, and a later stop only shortens it or drops it before it starts.
// `AsrMixer::render` is this with every command pushed first.
pub struct AsrStream {
	mixer: AsrMixer,
	grid: PassGrid,
	graph: BusGraph,
	slots: Vec<Slot>,
	playbacks: Vec<Playback>,
	layers: Vec<std::ops::Range<usize>>, // each playback's voices
	// Every voice in play order, which is the order they are summed in; `None` once it has ended or
	// a stop came before it started
	voices: Vec<Option<Voice>>,
	waiting: BTreeSet<(u64, usize)>, // voices yet to start, by their first frame
	sounding: BTreeSet<usize>,
	spans: Vec<(u64, u64)>, // the frames of every voice that has ended
	planes: Vec<Vec<f32>>,
	live: Vec<bool>,
	block: usize,       // the next production block
	rendered: Vec<f32>, // the last block, interleaved
	handed: usize,      // how much of it has been handed out
	frames: u64,        // handed out in all
	last_tick: u64,
	played: BTreeSet<u32>, // every category a play was in
	approximated: Approximated,
}

impl AsrStream {
	pub fn config(&self) -> AsrConfig {
		self.mixer.cfg
	}

	// `AsrMixer::add_cue`, for a cue added while the stream runs
	pub fn add_cue(&mut self, program: CueProgram, waveforms: Vec<Waveform>) -> Result<usize> {
		self.mixer.add_cue(program, waveforms)
	}

	// The earliest tick a command can still be pushed for: a pass is under way once its first block
	// is rendered.
	pub fn next_tick(&self) -> u64 {
		match self.block {
			0 => 0,
			n => self.grid.pass_of(self.grid.block(n - 1)) as u64 + 1,
		}
	}

	// Frames handed out so far
	pub fn frames(&self) -> u64 {
		self.frames
	}

	pub fn approximated(&self) -> &Approximated {
		&self.approximated
	}

	pub fn into_mixer(self) -> AsrMixer {
		self.mixer
	}

	// One command, on a tick no earlier than `next_tick` or the last command's
	pub fn push(&mut self, command: &Command) -> Result<()> {
		let fail =
			|reason: String| Error::Command { tick: command.tick, player: command.player, reason };
		if command.tick < self.next_tick() {
			return Err(fail(format!("tick {} is already rendering", self.next_tick() - 1)));
		}
		if command.tick < self.last_tick {
			return Err(fail(format!("it comes after a command on tick {}", self.last_tick)));
		}
		self.last_tick = command.tick;
		let players = self.slots.len();
		let rate = self.mixer.cfg.rate as i64;
		let held = self.mixer.cfg.player_held_frames();
		let grid = &self.grid;
		let slot =
			(self.slots.get_mut(command.player)).ok_or_else(|| fail(format!("{players} players")))?;
		let issued_at = grid.start(command.tick as usize);
		let playback = match &command.op {
			Op::Play(play) => {
				// The game stops a player before every play on it.
				if slot.active.is_some() || slot.held.is_some() {
					return Err(fail("the player is already playing".into()));
				}
				let program = self.mixer.program(play.sheet, &play.cue)?;
				let pitches = draw_pitches(&program, &mut slot.rng);
				let state = PlayState {
					sheet: play.sheet,
					program,
					pitches,
					start_time_ms: play.start_time_ms,
					controls: slot.controls.clone(),
				};
				if play.prepare {
					slot.held = Some(state);
					return Ok(());
				}
				let occupied = slot.occupied_until.is_some_and(|f| issued_at < f);
				let onset_pass = command.tick as usize + usize::from(!occupied);
				Playback {
					play: state,
					onset_pass,
					onset: grid.start(onset_pass),
					origin_us: grid.clock_after(command.tick as usize),
					release: None,
				}
			}
			Op::Pause(false) => {
				// The release sounds where its own tick's pass begins. How a held cue's later layers
				// and AISACs run is unread; a music cue has neither.
				let Some(state) = slot.held.take() else { return Ok(()) };
				let onset_pass = command.tick as usize;
				Playback {
					play: state,
					onset_pass,
					onset: grid.start(onset_pass),
					origin_us: grid.clock_after(onset_pass),
					release: None,
				}
			}
			Op::Pause(true) => return Err(fail("pausing a sounding play is not modeled".into())),
			Op::Aisac { control_id, value } => {
				slot.controls.insert(*control_id, *value);
				return Ok(());
			}
			Op::Stop => {
				slot.held = None;
				let Some(i) = slot.active.take() else { return Ok(()) };
				let playback = &mut self.playbacks[i];
				playback.release = Some(issued_at);
				let end =
					playback.natural_end(grid, rate).min(issued_at + PRODUCTION_CHUNK).max(playback.onset);
				slot.occupied_until = Some(end + held);
				// A note-on due at or after the pass a stop lands in never happens.
				for v in self.layers[i].clone() {
					let Some(voice) = &mut self.voices[v] else { continue };
					if voice.start >= issued_at {
						self.waiting.remove(&(voice.start, v));
						self.voices[v] = None;
					} else {
						voice.last = voice.last.min(issued_at + PRODUCTION_CHUNK);
					}
				}
				return Ok(());
			}
		};
		let index = self.playbacks.len();
		slot.active = Some(index);
		slot.occupied_until = Some(playback.natural_end(grid, rate) + held);

		let mut voices = Vec::new();
		self.mixer.voices(index, &playback, grid, &self.graph, &mut voices, &mut self.approximated)?;
		let first = self.voices.len();
		for voice in voices {
			self.waiting.insert((voice.start, self.voices.len()));
			self.voices.push(Some(voice));
		}
		self.layers.push(first..self.voices.len());
		self.played.extend(playback.play.program.categories());
		let acf = &self.mixer.acf;
		let index_played =
			|i: usize| acf.categories.get(i).is_some_and(|c| self.played.contains(&c.id));
		for r in &acf.reacts {
			if index_played(r.source) && index_played(r.dest) {
				let name = |i: usize| acf.categories.get(i).map_or("?", |c| c.name.as_str());
				self.approximated.reacts.insert(format!("{} ducks {}", name(r.source), name(r.dest)));
			}
		}
		self.playbacks.push(playback);
		Ok(())
	}

	// The next `out.len() / channels` frames, interleaved
	pub fn fill(&mut self, out: &mut [f32]) {
		let mut done = 0;
		while done < out.len() {
			if self.handed == self.rendered.len() {
				self.render_block();
			}
			let n = (self.rendered.len() - self.handed).min(out.len() - done);
			out[done..done + n].copy_from_slice(&self.rendered[self.handed..self.handed + n]);
			self.handed += n;
			done += n;
		}
		self.frames += (out.len() / self.mixer.cfg.channels) as u64;
	}

	fn render_block(&mut self) {
		let channels = self.mixer.cfg.channels;
		let (b0, b1) = (self.grid.block(self.block), self.grid.block(self.block + 1));
		self.block += 1;
		let len = (b1 - b0) as usize;
		for p in &mut self.planes {
			p[..len * channels].fill(0.0);
		}
		self.live.fill(false);

		while let Some(&(start, v)) = self.waiting.first()
			&& start < b1
		{
			self.waiting.pop_first();
			self.sounding.insert(v);
		}
		let voices = &mut self.voices;
		let spans = &mut self.spans;
		self.sounding.retain(|&v| {
			let voice = voices[v].as_ref().expect("a sounding voice");
			if voice.last > b0 {
				return true;
			}
			spans.push((voice.start, voice.last));
			voices[v] = None;
			false
		});
		for &v in &self.sounding {
			let voice = voices[v].as_mut().expect("a sounding voice");
			let playback = &self.playbacks[voice.playback];
			render_voice(voice, playback, &self.grid, b0, b1, channels, &mut self.planes, &mut self.live);
		}

		for n in 0..self.graph.order.len() {
			let i = self.graph.order[n];
			let (graph, planes, live) = (&mut self.graph, &mut self.planes, &mut self.live);
			run_bus(graph, i, len, channels, planes, live, &mut self.approximated);
		}
		self.rendered.clear();
		self.rendered.extend_from_slice(&self.planes[self.graph.root][..len * channels]);
		self.handed = 0;
	}

	// The most layers sounding at once in the first `frames` frames
	fn peak_concurrent_layers(&self, frames: u64) -> usize {
		let open = self.voices.iter().flatten().map(|v| (v.start, v.last));
		let spans = self.spans.iter().copied().chain(open).filter(|&(start, _)| start < frames);
		let mut edges: Vec<(u64, i32)> = Vec::new();
		for (start, last) in spans {
			edges.push((start, 1));
			edges.push((last.min(frames), -1));
		}
		edges.sort_unstable();
		let (mut now, mut peak) = (0i32, 0i32);
		for (_, delta) in &edges {
			now += delta;
			peak = peak.max(now);
		}
		peak as usize
	}
}

// What the mixer refuses: anything a program carries that it would otherwise ignore
fn renderable(program: &CueProgram) -> Result<()> {
	let refuse = |what: String| Err(Error::unrenderable(format!("cue {}: {what}", program.name)));
	if program.sequence_type != SequenceType::Polyphonic {
		return refuse(format!("a {:?} sequence", program.sequence_type));
	}
	if !program.action_tracks.is_empty() {
		return refuse("action tracks".into());
	}
	if !program.local_aisacs.is_empty() || !program.global_aisacs.is_empty() {
		return refuse("sequence AISACs".into());
	}
	for t in &program.tracks {
		if !t.nested.is_empty() {
			return refuse("a nested sequence".into());
		}
		if !t.global_aisacs.is_empty() {
			return refuse("track global AISACs".into());
		}
		if t.layers.iter().any(|l| !l.local_aisacs.is_empty()) {
			return refuse("synth AISACs".into());
		}
	}
	for p in program.all_params() {
		match p {
			// `MaxPitch` only reserves resampler headroom.
			CueParam::Volume(_)
			| CueParam::Pitch(_)
			| CueParam::RandomPitch { .. }
			| CueParam::MaxPitch(_)
			| CueParam::Categories(_)
			| CueParam::BusSendLevel { .. } => {}
			other => return refuse(format!("{other:?}")),
		}
	}
	Ok(())
}

// The type-1 AISAC value a playback's track has in pass `pass`. A stop is the same parameter going
// to zero, so a stopping voice ignores the track's new AISAC value.
fn automation(
	playback: &Playback,
	grid: &PassGrid,
	graphs: &[Vec<(f32, u16)>],
	pass: usize,
) -> f32 {
	if playback.stopping(grid, pass) {
		return 0.0;
	}
	if graphs.is_empty() {
		return 1.0;
	}
	let cursor =
		(grid.cursor_us(pass) - playback.origin_us).div_euclid(1000) + playback.play.start_time_ms;
	graphs.iter().map(|g| graph_at(g, cursor as f64)).product::<f64>() as f32
}

// A voice's frames in `[b0, b1)` into its buses' input planes. A bus a send reaches with a
// non-zero gain, at the block's start or at the end of its crossfade, is live this block.
#[allow(clippy::too_many_arguments)]
fn render_voice(
	voice: &mut Voice,
	playback: &Playback,
	grid: &PassGrid,
	b0: u64,
	b1: u64,
	channels: usize,
	planes: &mut [Vec<f32>],
	live: &mut [bool],
) {
	let from = voice.start.max(b0);
	let to = voice.last.min(b1);
	let source_frames = voice.waveform.pcm.len() / 2;
	for frame in from..to {
		while frame >= voice.pass_end {
			voice.pass += 1;
			voice.pass_end = grid.start(voice.pass + 1);
			voice.previous = voice.target;
			voice.target = automation(playback, grid, &voice.graphs, voice.pass);
		}
		let automation = crossfade(voice.previous, voice.target, frame - grid.start(voice.pass));
		if frame == from {
			for &(bus, level) in &voice.sends {
				let now = level * ((voice.gain * automation) * voice.category_volume);
				let then = level * ((voice.gain * voice.target) * voice.category_volume);
				live[bus] |= now != 0.0 || then != 0.0;
			}
		}

		let (mut l, mut r) =
			sample(&voice.waveform.pcm, source_frames, voice.step, frame - voice.start, voice.skip);
		// The voice's own pulse lands on its decoded planes, before its send gains.
		let pulse = grid.production_pulse(frame);
		l += pulse;
		r += pulse;
		let base = (frame - b0) as usize * channels;
		// The send gain is formed as `level * volume` and applied once.
		let overall = (voice.gain * automation) * voice.category_volume;
		for &(bus, level) in &voice.sends {
			let g = level * overall;
			if channels >= 2 {
				planes[bus][base] += l * g;
				planes[bus][base + 1] += r * g;
			}
		}
	}
}

// One bus for one block: its effects, its pulse and its sends, if it is live. The root keeps its
// planes, which are the output; at unit volume with no effect it adds no pulse.
#[allow(clippy::too_many_arguments)]
fn run_bus(
	graph: &mut BusGraph,
	i: usize,
	len: usize,
	channels: usize,
	planes: &mut [Vec<f32>],
	live: &mut [bool],
	approximated: &mut Approximated,
) {
	let input_live = live[i];
	let mut output_live = input_live;
	let mut tail = false;
	let bus = &mut graph.buses[i];
	for effect in &mut bus.effects {
		match effect {
			BusEffect::Matrix(m) => {
				if input_live {
					apply_matrix(m, &mut planes[i][..len * channels], channels);
				}
			}
			BusEffect::Echo { since_input, tail: length } => {
				if input_live {
					*since_input = 0;
					approximated.effects.insert(format!("CRIWARE/Echo on {}", bus.name));
				} else if *since_input <= *length {
					*since_input += len as u32;
					tail = true;
				}
			}
			BusEffect::PassThrough(name) => {
				if input_live {
					approximated.effects.insert(format!("{name} on {}", bus.name));
				}
			}
		}
	}
	output_live |= tail;
	if bus.is_root {
		let unity = bus.volume == 1.0 && bus.effects.is_empty();
		if !unity {
			approximated
				.effects
				.insert(format!("a root bus {} at volume {} with effects", bus.name, bus.volume));
			for s in &mut planes[i][..len * channels] {
				*s *= bus.volume;
			}
			pulse(&mut planes[i][..len * channels], channels);
		}
		return;
	}
	if !output_live {
		return;
	}
	pulse(&mut planes[i][..len * channels], channels);
	let sends = bus.sends.clone();
	for (dest, gain) in sends {
		if gain == 0.0 {
			continue;
		}
		live[dest] = true;
		let (src, dst) = two(planes, i, dest);
		// Unit gain is a plain add; any other is `src * gain + dst`, unfused.
		if gain == 1.0 {
			for (d, s) in dst[..len * channels].iter_mut().zip(&src[..len * channels]) {
				*d += *s;
			}
		} else {
			for (d, s) in dst[..len * channels].iter_mut().zip(&src[..len * channels]) {
				*d += *s * gain;
			}
		}
	}
}

// `+2^-20` on frame 0 and `-2^-20` on frame 1 of every plane
fn pulse(block: &mut [f32], channels: usize) {
	for (frame, sign) in [(0, 1.0), (1, -1.0)] {
		for s in block.iter_mut().skip(frame * channels).take(channels) {
			*s += sign * ASR_ANTI_DENORMAL;
		}
	}
}

// `out[o] = sum of in[i] * m[i][o]` over the non-zero gains, the first term not added to zero
fn apply_matrix(m: &[[f32; 8]; 8], block: &mut [f32], channels: usize) {
	let width = channels.min(8);
	for frame in block.chunks_exact_mut(channels) {
		let input: [f32; 8] = std::array::from_fn(|c| if c < width { frame[c] } else { 0.0 });
		for (o, out) in frame.iter_mut().enumerate().take(width) {
			let mut acc: Option<f32> = None;
			for (i, &x) in input.iter().enumerate().take(width) {
				let g = m[i][o];
				if g != 0.0 {
					let term = if g == 1.0 { x } else { x * g };
					acc = Some(acc.map_or(term, |a| a + term));
				}
			}
			*out = acc.unwrap_or(0.0);
		}
	}
}

fn two(planes: &mut [Vec<f32>], a: usize, b: usize) -> (&[f32], &mut [f32]) {
	if a < b {
		let (x, y) = planes.split_at_mut(b);
		(&x[a], &mut y[0])
	} else {
		let (x, y) = planes.split_at_mut(a);
		(&y[0], &mut x[b])
	}
}

// A parameter write crosses linearly to its value over the first `PRODUCTION_CHUNK` frames of the
// pass carrying it; a stop is this with target 0.
fn crossfade(previous: f32, target: f32, into: u64) -> f32 {
	if previous == target || into >= PRODUCTION_CHUNK {
		return target;
	}
	previous + (target - previous) * into as f32 / PRODUCTION_CHUNK as f32
}

// A voice's step: its frequency, the waveform's rate times `2^(cents/1200)` in binary32 and
// truncated to whole hertz, at most 8 times the output rate, as a 20.12 fixed-point ratio to it. A
// voice at the output's rate and no pitch steps by exactly 1.
fn step(rate: u32, output_rate: u32, cents: Option<f32>) -> u32 {
	let ratio = cents.map_or(1.0, |c| 2f32.powf(c * 0.000_833_333_35));
	let frequency = ((rate as f32 * ratio) as i32).max(1) as i64;
	let output_rate = (output_rate as i64).max(1);
	((frequency.min(8 * output_rate) << 12) / output_rate).max(1) as u32
}

const UNIT_STEP: u32 = 4096;

// How many output frames a voice makes of `source` frames. Each one interpolates toward the next
// source frame, so a voice stepping slower than 1 stops `4096 / step` frames short.
fn resampled_frames(source: u64, step: u32) -> u64 {
	if step == UNIT_STEP {
		return source;
	}
	let short = if step < UNIT_STEP { (UNIT_STEP / step) as u64 } else { 0 };
	(source * UNIT_STEP as u64 / step as u64).saturating_sub(short)
}

// The source pair for output frame `offset`: a lookup at a unit step, else the line between the
// two source frames around `offset · step`, at its fraction in 1/4096ths.
fn sample(pcm: &[f32], source_frames: usize, step: u32, offset: u64, skip: usize) -> (f32, f32) {
	if step == UNIT_STEP {
		let i = skip + offset as usize;
		return (pcm[i * 2], pcm[i * 2 + 1]);
	}
	let position = offset * step as u64;
	let a = (skip + (position >> 12) as usize).min(source_frames - 1);
	let b = (a + 1).min(source_frames - 1);
	let t = (position & 0xfff) as f32 * (1.0 / 4096.0);
	let lerp = |c: usize| (pcm[b * 2 + c] - pcm[a * 2 + c]) * t + pcm[a * 2 + c];
	(lerp(0), lerp(1))
}

// Each layer's pitch, the innermost scope's. A random scope draws from the player's generator
// once: the sequence's for the whole cue, then each track's in track order.
fn draw_pitches(program: &CueProgram, rng: &mut [u32; 4]) -> Vec<Vec<Option<f32>>> {
	let mut draw_scope = |params: &[CueParam]| -> Option<f32> {
		params.iter().find_map(|p| match *p {
			CueParam::Pitch(cents) => Some(cents),
			CueParam::RandomPitch { center_cents, range_cents } => {
				Some(draw(rng, center_cents, range_cents))
			}
			_ => None,
		})
	};
	let sequence = draw_scope(&program.params);
	let mut out = Vec::with_capacity(program.tracks.len());
	for track in &program.tracks {
		let track_pitch = draw_scope(&track.params).or(sequence);
		out.push(track.layers.iter().map(|layer| draw_scope(&layer.params).or(track_pitch)).collect());
	}
	out
}

// The library's generator is Marsaglia's xorshift128, seeded as below.
fn seeded(seed: u32) -> [u32; 4] {
	let mut s = [0u32; 4];
	s[0] = 1_812_433_253u32.wrapping_mul(seed ^ (seed >> 30));
	for i in 1..4 {
		s[i] = 1_812_433_253u32.wrapping_mul(s[i - 1] ^ (s[i - 1] >> 30)).wrapping_add(i as u32);
	}
	s
}

fn next(s: &mut [u32; 4]) -> u32 {
	let t = s[0] ^ (s[0] << 11);
	s[0] = s[1];
	s[1] = s[2];
	s[2] = s[3];
	s[3] = s[3] ^ t ^ ((t ^ (s[3] >> 11)) >> 8);
	s[3]
}

// A pitch within `center ± range`, in binary32 in the native order, the draw scaled by 2^-31. The
// ±2400 bounds are the interpreter's.
fn draw(rng: &mut [u32; 4], center: f32, range: f32) -> f32 {
	const BOUND: f32 = 2400.0;
	if range == 0.0 {
		return center.clamp(-BOUND, BOUND);
	}
	let lo = (center - range).max(-BOUND);
	let width = (center + range).min(BOUND) - lo;
	let n = next(rng) % 0x8000_0000;
	((n as f32 * width) * f32::from_bits(0x3000_0000) + lo).clamp(-BOUND, BOUND)
}

fn volume_of(params: &[CueParam]) -> Option<f32> {
	params.iter().find_map(|p| match p {
		CueParam::Volume(v) => Some(*v),
		_ => None,
	})
}

// A type-1 local AISAC graph at a control value in ms; 10000 is unity, and curve code 100 is read
// as linear.
fn graph_at(points: &[(f32, u16)], x: f64) -> f64 {
	let Some(first) = points.first() else { return 1.0 };
	if x <= first.0 as f64 {
		return first.1 as f64 * 1e-4;
	}
	for w in points.windows(2) {
		let (x0, y0) = (w[0].0 as f64, w[0].1 as f64 * 1e-4);
		let (x1, y1) = (w[1].0 as f64, w[1].1 as f64 * 1e-4);
		if x <= x1 {
			return if x1 > x0 { y0 + (y1 - y0) * (x - x0) / (x1 - x0) } else { y1 };
		}
	}
	points[points.len() - 1].1 as f64 * 1e-4
}

// A global AISAC graph at control `c`, in `f32`: `y0 + ((c - x0) / (x1 - x0)) * (y1 - y0)`, each
// destination over 10000, clamped to the end points
fn global_graph_at(points: &[(f32, u16)], c: f32) -> f32 {
	let y = |d: u16| d as i16 as f32 / 10000.0;
	let (Some(first), Some(last)) = (points.first(), points.last()) else { return 1.0 };
	if c <= first.0 {
		return y(first.1);
	}
	if c >= last.0 {
		return y(last.1);
	}
	for w in points.windows(2) {
		let ((x0, d0), (x1, d1)) = (w[0], w[1]);
		if c <= x1 && x1 > x0 {
			return y(d0) + ((c - x0) / (x1 - x0)) * (y(d1) - y(d0));
		}
	}
	y(last.1)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cfg() -> AsrConfig {
		AsrConfig {
			rate: 48_000,
			server_frequency: 120,
			channels: 8,
			release_quantum_ticks: 1,
			players: 4,
			seed: 0,
			keycode: None,
		}
	}

	// Each pass's production blocks, by length
	fn blocks_of(cfg: &AsrConfig, passes: usize) -> Vec<Vec<u32>> {
		let grid = PassGrid::new(cfg);
		let mut blocks = vec![Vec::new(); passes];
		let mut n = 0;
		while grid.block(n) < grid.start(passes) {
			let pass = grid.pass_of(grid.block(n));
			blocks[pass].push((grid.block(n + 1) - grid.block(n)) as u32);
			n += 1;
		}
		blocks
	}

	#[test]
	fn pass_grid() {
		let cfg = cfg();
		assert_eq!(cfg.frames_per_tick(), 400);
		assert_eq!(cfg.frames_per_pass(), 448);

		let grid = PassGrid::new(&cfg);
		let starts = [
			0, 448, 864, 1248, 1664, 2048, 2464, 2848, 3264, 3648, 4064, 4448, 4864, 5248, 5664, 6048,
			6464, 6848, 7264, 7648, 8064,
		];
		assert_eq!((0..=20).map(|p| grid.start(p)).collect::<Vec<_>>(), starts);
		assert_eq!(
			blocks_of(&cfg, 4),
			[vec![256, 192], vec![256, 160], vec![256, 128], vec![256, 160]]
		);
		let blocks = (0..8).map(|n| grid.block(n)).collect::<Vec<_>>();
		assert_eq!(blocks, [0, 256, 448, 704, 864, 1120, 1248, 1504]);
		assert_eq!(grid.pass_of(447), 0);
		assert_eq!(grid.pass_of(448), 1);
	}

	// At 60 Hz, `p * 800 + (p odd ? 64 : 32)` falls out of the same simulation.
	#[test]
	fn pass_grid_60_hz() {
		let cfg = AsrConfig { server_frequency: 60, ..cfg() };
		assert_eq!(cfg.frames_per_tick(), 800);
		assert_eq!(cfg.frames_per_pass(), 864);
		let grid = PassGrid::new(&cfg);
		for p in 1..=40u64 {
			let lead = if p % 2 == 1 { 64 } else { 32 };
			assert_eq!(grid.start(p as usize), p * 800 + lead, "pass {p}");
		}
		assert_eq!(
			blocks_of(&cfg, 3),
			[vec![256, 256, 256, 96], vec![256, 256, 256], vec![256, 256, 256, 64]]
		);
	}

	// Rounding would put the 166,666 µs pass at 167 ms.
	#[test]
	fn master_clock() {
		let grid = PassGrid::new(&cfg());
		assert_eq!(grid.clock_after(20), 166_666);
		assert_eq!(grid.clock_after(7), 58_333);
		assert_eq!(grid.cursor_us(23).div_euclid(1000), 200);
		assert_eq!(grid.cursor_us(22).div_euclid(1000), 191);
		assert_eq!(grid.start(23), 9_248);
	}

	// 50 ms at 0.3 and 300 ms at 0.1, at 48 kHz
	#[test]
	fn echo_tails() {
		assert_eq!(echo_tail(48_000, 50.0, 0.3), 23_123);
		assert_eq!(echo_tail(48_000, 300.0, 0.1), 79_414);
	}

	#[test]
	fn graphs() {
		let points = [(150.0f32, 20000u16), (231.0, 10200)];
		assert!((graph_at(&points, 0.0) - 2.0).abs() < 1e-12);
		assert!((graph_at(&points, 300.0) - 1.02).abs() < 1e-12);
		assert!((graph_at(&points, 190.5) - 1.51).abs() < 1e-3);
		let linear = [(0.0f32, 0u16), (1.0, 10000)];
		for c in [0.0f32, 0.25, 0.5, 0.71, 1.0] {
			assert_eq!(global_graph_at(&linear, c), c);
		}
		assert_eq!(global_graph_at(&linear, 2.0), 1.0);
	}

	#[test]
	fn crossfades() {
		assert_eq!(crossfade(1.0, 0.0, 0), 1.0);
		assert_eq!(crossfade(1.0, 0.0, 3), 1.0 - 3.0 / 256.0);
		assert_eq!(crossfade(1.0, 0.0, 255), 1.0 / 256.0);
		assert_eq!(crossfade(1.0, 0.0, 256), 0.0);
		assert_eq!(crossfade(0.65, 0.65, 7), 0.65);
	}

	#[test]
	fn matrices() {
		let mut m = [[0.0f32; 8]; 8];
		m[0][2] = 1.0;
		m[1][3] = 1.0;
		m[1][2] = 0.5;
		let mut block = [1.0, 2.0, 9.0, 9.0, 0.0, 0.0, 0.0, 0.0];
		apply_matrix(&m, &mut block, 8);
		assert_eq!(block, [0.0, 0.0, 2.0, 2.0, 0.0, 0.0, 0.0, 0.0]);
	}

	// The seed-0 state, and player 0's first `30 ± 255` draws and players 1 to 3's first
	#[test]
	fn generator() {
		assert_eq!(seeded(0), [0, 1, 0x6C07_8967, 0x714A_CB41]);
		let mut global = seeded(0);
		let players: Vec<[u32; 4]> = (0..4)
			.map(|_| {
				let a = seeded(next(&mut global) % 0x8000_0000);
				next(&mut global);
				a
			})
			.collect();
		let mut p0 = players[0];
		let drawn: Vec<f32> = (0..4).map(|_| draw(&mut p0, 30.0, 255.0)).collect();
		for (got, want) in drawn.iter().zip([-68.97, 76.28, -123.43, -217.77]) {
			assert!((got - want).abs() < 0.01, "{drawn:?}");
		}
		for (p, want) in [(1, 155.96), (2, -173.63), (3, 70.62)] {
			let got = draw(&mut players[p].clone(), 30.0, 255.0);
			assert!((got - want).abs() < 0.01, "player {p}: {got}");
		}
	}

	// A live player renders on the audio device's thread.
	#[test]
	fn a_stream_can_move_to_another_thread() {
		fn send<T: Send>() {}
		send::<AsrStream>();
	}

	#[test]
	fn steps() {
		assert_eq!(step(48_000, 48_000, None), UNIT_STEP);
		assert_eq!(step(48_000, 48_000, Some(0.0)), UNIT_STEP);
		assert_eq!(step(44_100, 48_000, None), 3763);
		// 48000 · 2^(135/1200) = 51,892.8 Hz, truncated before the division
		assert_eq!(step(48_000, 48_000, Some(135.0)), (51_892 << 12) / 48_000);
		assert_eq!(step(48_000, 48_000, Some(1200.0)), 8192);
		assert_eq!(step(48_000, 48_000, Some(4800.0)), 8 * UNIT_STEP);
	}

	#[test]
	fn resampled_lengths() {
		assert_eq!(resampled_frames(1000, UNIT_STEP), 1000);
		assert_eq!(resampled_frames(1000, 8192), 500);
		// 4096 / 3763 = 1 frame short of 1000 · 4096 / 3763 = 1088.5
		assert_eq!(resampled_frames(1000, 3763), 1087);
	}

	#[test]
	fn interpolation() {
		let pcm = [0.0, 1.0, 1.0, -1.0, 0.5, 0.25];
		assert_eq!(sample(&pcm, 3, UNIT_STEP, 1, 0), (1.0, -1.0));
		// Halfway between frames 0 and 1, then a quarter of the way from 1 to 2
		assert_eq!(sample(&pcm, 3, 2048, 1, 0), (0.5, 0.0));
		assert_eq!(sample(&pcm, 3, 5120, 1, 0), (0.875, -0.6875));
	}

	#[test]
	fn a_resampling_voice_ends_with_its_last_pull() {
		let grid = PassGrid::new(&cfg());
		// 1060 frames at 1.5 could make 706, but the pull whose 8-frame lookahead reaches the last
		// source frame is the block ending at 704, and the voice ends with it.
		assert_eq!(resampled_frames(1060, 6144), 706);
		assert_eq!(grid.voice_frames(0, 1060, 6144), 704);
		// From 448 that pull is the block ending at 1120, past all 706.
		assert_eq!(grid.voice_frames(448, 1060, 6144), 706);
		assert_eq!(grid.voice_frames(0, 1060, UNIT_STEP), 1060);
	}
}
