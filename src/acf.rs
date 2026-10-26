// ACF project settings: a one-row `@UTF` table, as an ACB is, holding the categories, the DSP bus
// settings, the global AISACs, REACT rules and selectors an application mixes its cues with. Two
// layouts are read: one names each effect (`DspName`), an older one numbers it (`FxType`).

use crate::acb::parse_commands;
use crate::acb::program::{CommandTable, CueParam, Graph, decode_param};
use crate::error::{Error, Format, Result, ensure};
use crate::utf::Utf;

pub struct Acf {
	pub name: String,
	pub version: u32,
	pub version_string: String,
	pub categories: Vec<Category>,
	pub dsp_settings: Vec<DspSetting>,
	// The setting the runtime attaches when the application names none
	pub default_dsp_setting: String,
	pub aisac_controls: Vec<AisacControl>,
	pub global_aisacs: Vec<GlobalAisac>,
	// `BusNameIndexListForAisacGraphType`: the bus names an AISAC graph's type counts through
	pub aisac_graph_buses: Vec<String>,
	pub reacts: Vec<React>,
	pub selectors: Vec<Selector>,
	pub voice_limit_groups: Vec<VoiceLimitGroup>,
	root: Utf,
}

#[derive(Debug, Clone)]
pub struct Category {
	pub index: usize, // what `React` and the category names refer to
	pub id: u32,      // what a cue's `CueParam::Categories` lists
	pub name: String,
	pub group: u32,
	pub params: Vec<CueParam>,
	pub global_aisacs: Vec<usize>, // into `Acf::global_aisacs`
}

impl Category {
	pub fn volume(&self) -> Option<f32> {
		self.params.iter().find_map(|p| match p {
			CueParam::Volume(v) => Some(*v),
			_ => None,
		})
	}
}

#[derive(Debug, Clone)]
pub struct DspSetting {
	pub name: String,
	pub buses: Vec<Bus>,
}

impl DspSetting {
	pub fn bus(&self, name: &str) -> Option<&Bus> {
		self.buses.iter().find(|b| b.name == name)
	}
}

#[derive(Debug, Clone)]
pub struct Bus {
	pub number: u32,
	pub name: String,
	pub volume: f32,
	pub pan3d_volume: f32,
	pub pan3d_distance: f32,
	pub effects: Vec<Effect>, // in processing order
	pub sends: Vec<BusSend>,
}

#[derive(Debug, Clone)]
pub struct BusSend {
	pub bus_number: u32,
	pub bus_name: String,
	pub level: f32,
	pub kind: i64, // `BusLinkTable.Type`; 2 everywhere so far
}

#[derive(Debug, Clone)]
pub struct Effect {
	pub kind: EffectKind,
	pub bypass: bool,
	pub parameters: Vec<u8>, // as stored; the layout is the effect's
	pub config: Vec<u8>,     // `ConfigParameters`, fixed when the bus is built
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectKind {
	Named(String), // `DspName`, such as `CRIWARE/Matrix`
	Numbered(i64), // the older `FxType`
}

impl Effect {
	// `CRIWARE/Matrix`, or its older number
	pub fn name(&self) -> Option<&str> {
		match &self.kind {
			EffectKind::Named(n) => Some(n),
			EffectKind::Numbered(n) => match n {
				3 => Some("CRIWARE/Biquad"),
				4 => Some("CRIWARE/Bandpass"),
				8 => Some("CRIWARE/3BandsEQ"),
				14 => Some("CRIWARE/I3DL2Reverb"),
				17 => Some("CRIWARE/Matrix"),
				18 => Some("CRIWARE/32BandsEQ"),
				_ => None,
			},
		}
	}

	// The parameters as big-endian floats, the layout of every named effect
	pub fn floats(&self) -> Vec<f32> {
		self.parameters.as_chunks::<4>().0.iter().map(|b| f32::from_be_bytes(*b)).collect()
	}

	// A `Matrix` effect's gains, `[input][output]`. Both layouts store 64 floats first; the older
	// one adds a byte after them.
	pub fn matrix(&self) -> Option<[[f32; 8]; 8]> {
		if self.name() != Some("CRIWARE/Matrix") || self.parameters.len() < 256 {
			return None;
		}
		let v: Vec<f32> =
			self.parameters[..256].as_chunks::<4>().0.iter().map(|b| f32::from_be_bytes(*b)).collect();
		Some(std::array::from_fn(|i| std::array::from_fn(|o| v[i * 8 + o])))
	}
}

#[derive(Debug, Clone)]
pub struct AisacControl {
	pub id: u32,
	pub name: String,
}

#[derive(Debug, Clone)]
pub struct GlobalAisac {
	pub index: usize,
	pub name: String,
	pub control_id: u32,
	// `DefaultControl`, when `DefaultControlFlag` is set: the control's value until one is written
	pub default_control: Option<f32>,
	pub auto_modulation_index: Option<usize>,
	pub graphs: Vec<Graph>,
}

#[derive(Debug, Clone)]
pub struct React {
	pub source: usize, // `Category::index`
	pub dest: usize,
	pub level: f32,
	pub decrement_ms: u32,
	pub increment_ms: u32,
	pub hold_kind: u32,
	pub hold_ms: u32,
	pub start_curve: (u32, u32), // type and strength
	pub end_curve: (u32, u32),
}

#[derive(Debug, Clone)]
pub struct Selector {
	pub name: String,
	pub labels: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VoiceLimitGroup {
	pub name: String,
	pub max: u32,
}

impl Acf {
	pub fn open(path: &std::path::Path) -> Result<Self> {
		let buf =
			std::fs::read(path).map_err(|source| Error::Io { path: path.to_path_buf(), source })?;
		Self::from_bytes(&buf)
	}

	pub fn from_bytes(buf: &[u8]) -> Result<Self> {
		let root = Utf::parse(buf)?;
		ensure!(!root.rows.is_empty(), Error::malformed(Format::Acf, "the root table has no rows"));
		let t = |name: &str| -> Result<Utf> {
			match root.get_table(0, name) {
				Some(t) => t,
				None => Ok(Utf::empty(name)),
			}
		};
		let bus_names = column_strings(&t("BusNameTable")?, "StringValue");
		let strings = column_strings(&t("StringValueTable")?, "StringValue");
		let graphs = t("GraphTable")?;

		let global = t("GlobalAisacTable")?;
		let mut global_names = vec![String::new(); global.rows.len()];
		let name_t = t("GlobalAisacNameTable")?;
		for r in 0..name_t.rows.len() {
			let i = name_t.get_i64(r, "Index").unwrap_or(0) as usize;
			if let Some(n) = global_names.get_mut(i) {
				*n = name_t.get_str(r, "Name").unwrap_or_default().to_string();
			}
		}
		let global_aisacs = (0..global.rows.len())
			.map(|i| GlobalAisac {
				index: i,
				name: global_names[i].clone(),
				control_id: global.get_i64(i, "ControlId").unwrap_or(0) as u32,
				default_control: (global.get_i64(i, "DefaultControlFlag").unwrap_or(0) != 0)
					.then(|| global.get_f32(i, "DefaultControl").unwrap_or(0.0)),
				auto_modulation_index: index(global.get_i64(i, "AutoModulationIndex")),
				graphs: global.get_indexes(i, "GraphIndexes").iter().map(|&g| graph(&graphs, g)).collect(),
			})
			.collect();

		let category = t("CategoryTable")?;
		let commands = t("CommandTable")?;
		let mut category_names = vec![String::new(); category.rows.len()];
		let cn = t("CategoryNameTable")?;
		for r in 0..cn.rows.len() {
			let i = cn.get_i64(r, "Index").unwrap_or(0) as usize;
			if let Some(n) = category_names.get_mut(i) {
				*n = cn.get_str(r, "Name").unwrap_or_default().to_string();
			}
		}
		let categories = (0..category.rows.len())
			.map(|i| {
				let stream = index(category.get_i64(i, "CommandIndex"))
					.and_then(|c| commands.get_bytes(c, "Command"))
					.unwrap_or_default();
				let params = parse_commands(stream)
					.iter()
					.map(|c| decode_param(c, CommandTable::Category, &strings))
					.collect::<Result<_>>()?;
				let aisacs = category.get_indexes(i, "Aisacs");
				Ok(Category {
					index: i,
					id: category.get_i64(i, "Id").unwrap_or(0) as u32,
					name: category_names[i].clone(),
					group: category.get_i64(i, "GroupIndex").unwrap_or(0) as u32,
					params,
					// Each entry is `0x8000 | index`.
					global_aisacs: aisacs.iter().map(|&a| a & 0x7fff).collect(),
				})
			})
			.collect::<Result<_>>()?;

		let bus = t("BusTable")?;
		let fx = t("DspFxTable")?;
		let links = t("BusLinkTable")?;
		let setting = t("DspSettingTable")?;
		let dsp_settings = (0..setting.rows.len())
			.map(|s| {
				let first = setting.get_i64(s, "StartIndex").unwrap_or(0) as usize;
				let count = setting.get_i64(s, "NumBuses").unwrap_or(0) as usize;
				let buses = (first..first + count)
					.map(|b| read_bus(&bus, &fx, &links, &bus_names, b))
					.collect::<Result<_>>()?;
				Ok(DspSetting { name: setting.get_str(s, "Name").unwrap_or_default().to_string(), buses })
			})
			.collect::<Result<_>>()?;

		let controls = t("AisacControlIdTable")?;
		let aisac_controls = (0..controls.rows.len())
			.map(|r| AisacControl {
				id: controls.get_i64(r, "AisacControlId").unwrap_or(0) as u32,
				name: controls.get_str(r, "AisacControlName").unwrap_or_default().to_string(),
			})
			.collect();

		let graph_buses = t("BusNameIndexListForAisacGraphType")?;
		let aisac_graph_buses = (0..graph_buses.rows.len())
			.map(|r| {
				let i = graph_buses.get_i64(r, "Id").unwrap_or(0) as usize;
				bus_names.get(i).cloned().unwrap_or_default()
			})
			.collect();

		let react = t("ReactTable")?;
		let u = |r: usize, n: &str| react.get_i64(r, n).unwrap_or(0) as u32;
		let reacts = (0..react.rows.len())
			.map(|r| React {
				source: u(r, "Src") as usize,
				dest: u(r, "Dest") as usize,
				level: react.get_f32(r, "Level").unwrap_or(1.0),
				decrement_ms: u(r, "DecrementTime"),
				increment_ms: u(r, "IncrementTime"),
				hold_kind: u(r, "HoldType"),
				hold_ms: u(r, "HoldTime"),
				start_curve: (u(r, "StartCurveType"), u(r, "StartCurveStrength")),
				end_curve: (u(r, "EndCurveType"), u(r, "EndCurveStrength")),
			})
			.collect();

		let selector = t("SelectorTable")?;
		let selectors = (0..selector.rows.len())
			.map(|r| {
				let first = selector.get_i64(r, "LabelStartIndex").unwrap_or(0) as usize;
				let count = selector.get_i64(r, "NumLabels").unwrap_or(0) as usize;
				let labels = (first..first + count)
					.map(|i| {
						let reason = || format!("selector[{r}]: label StringValueTable[{i}] is absent");
						strings.get(i).cloned().ok_or_else(|| Error::malformed(Format::Acf, reason()))
					})
					.collect::<Result<_>>()?;
				Ok(Selector { name: selector.get_str(r, "Name").unwrap_or_default().to_string(), labels })
			})
			.collect::<Result<_>>()?;

		let voice = t("VoiceLimitGroupTable")?;
		let voice_limit_groups = (0..voice.rows.len())
			.map(|r| VoiceLimitGroup {
				name: voice.get_str(r, "GroupName").unwrap_or_default().to_string(),
				max: voice.get_i64(r, "MaxNumbers").unwrap_or(0) as u32,
			})
			.collect();

		let options = t("RegistrationOption")?;
		Ok(Self {
			name: root.get_str(0, "Name").unwrap_or_default().to_string(),
			version: root.get_i64(0, "Version").unwrap_or(0) as u32,
			version_string: root.get_str(0, "VersionString").unwrap_or_default().trim().to_string(),
			categories,
			dsp_settings,
			default_dsp_setting: options
				.get_str(0, "DefaultDspBusSettingName")
				.unwrap_or_default()
				.to_string(),
			aisac_controls,
			global_aisacs,
			aisac_graph_buses,
			reacts,
			selectors,
			voice_limit_groups,
			root,
		})
	}

	pub fn root(&self) -> &Utf {
		&self.root
	}

	pub fn category(&self, name: &str) -> Option<&Category> {
		self.categories.iter().find(|c| c.name == name)
	}
	pub fn category_by_id(&self, id: u32) -> Option<&Category> {
		self.categories.iter().find(|c| c.id == id)
	}
	pub fn dsp_setting(&self, name: &str) -> Option<&DspSetting> {
		self.dsp_settings.iter().find(|s| s.name == name)
	}
	pub fn global_aisac(&self, name: &str) -> Option<&GlobalAisac> {
		self.global_aisacs.iter().find(|a| a.name == name)
	}
	pub fn aisac_control(&self, name: &str) -> Option<&AisacControl> {
		self.aisac_controls.iter().find(|c| c.name == name)
	}
}

fn read_bus(bus: &Utf, fx: &Utf, links: &Utf, names: &[String], b: usize) -> Result<Bus> {
	ensure!(b < bus.rows.len(), Error::malformed(Format::Acf, format!("bus[{b}] out of range")));
	let name = |i: Option<i64>| -> Result<String> {
		let i = i.unwrap_or(0) as usize;
		let reason = || format!("bus[{b}]: BusNameTable[{i}] is absent");
		names.get(i).cloned().ok_or_else(|| Error::malformed(Format::Acf, reason()))
	};
	let range = |start: &str, count: &str, t: &Utf| -> Result<std::ops::Range<usize>> {
		let Some(first) = index(bus.get_i64(b, start)) else { return Ok(0..0) };
		let count = bus.get_i64(b, count).unwrap_or(0) as usize;
		ensure!(
			first + count <= t.rows.len(),
			Error::malformed(Format::Acf, format!("bus[{b}]: {start} runs past {}", t.name))
		);
		Ok(first..first + count)
	};
	let effects = range("StartFxIndex", "NumFxs", fx)?
		.map(|f| Effect {
			kind: match fx.get_str(f, "DspName") {
				Some(n) if !n.is_empty() => EffectKind::Named(n.to_string()),
				_ => EffectKind::Numbered(fx.get_i64(f, "FxType").unwrap_or(-1)),
			},
			bypass: fx.get_i64(f, "Bypass").unwrap_or(0) != 0,
			parameters: fx.get_bytes(f, "Parameters").unwrap_or_default().to_vec(),
			config: fx.get_bytes(f, "ConfigParameters").unwrap_or_default().to_vec(),
		})
		.collect();
	let sends = range("StartBusLinkIndex", "NumBusLinks", links)?
		.map(|l| {
			Ok(BusSend {
				bus_number: links.get_i64(l, "SendBusNo").unwrap_or(0) as u32,
				bus_name: name(links.get_i64(l, "BusNameIndex"))?,
				level: links.get_f32(l, "SendLevel").unwrap_or(0.0),
				kind: links.get_i64(l, "Type").unwrap_or(0),
			})
		})
		.collect::<Result<_>>()?;
	Ok(Bus {
		number: bus.get_i64(b, "BusNo").unwrap_or(0) as u32,
		name: name(bus.get_i64(b, "BusNameIndex"))?,
		volume: bus.get_f32(b, "Volume").unwrap_or(1.0),
		pan3d_volume: bus.get_f32(b, "Pan3dVolume").unwrap_or(1.0),
		pan3d_distance: bus.get_f32(b, "Pan3dDistance").unwrap_or(1.0),
		effects,
		sends,
	})
}

fn graph(t: &Utf, index: usize) -> Graph {
	let f = |n: &str| t.get_bytes(index, n).unwrap_or_default();
	let xs = f("Controls").as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c));
	let ys = f("Destinations").as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c));
	Graph { index, kind: t.get_i64(index, "Type").unwrap_or(-1), points: xs.zip(ys).collect() }
}

fn column_strings(t: &Utf, column: &str) -> Vec<String> {
	(0..t.rows.len()).map(|r| t.get_str(r, column).unwrap_or_default().to_string()).collect()
}

// `0xFFFF` and negative indexes are "none".
fn index(v: Option<i64>) -> Option<usize> {
	v.filter(|&i| (0..0xffff).contains(&i)).map(|i| i as usize)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn effect(kind: EffectKind, parameters: Vec<u8>) -> Effect {
		Effect { kind, bypass: false, parameters, config: Vec::new() }
	}

	// Input 0 to output 2 and input 1 to output 3, at `[input * 8 + output]`
	fn routing(extra: usize) -> Vec<u8> {
		let mut v = vec![0f32; 64];
		v[2] = 1.0;
		v[8 + 3] = 0.5;
		let mut b: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
		b.extend(std::iter::repeat_n(0, extra));
		b
	}

	#[test]
	fn matrix_in_both_layouts() {
		for e in [
			effect(EffectKind::Named("CRIWARE/Matrix".into()), routing(0)),
			effect(EffectKind::Numbered(17), routing(1)),
		] {
			let m = e.matrix().unwrap();
			assert_eq!((m[0][2], m[1][3], m[2][0]), (1.0, 0.5, 0.0));
		}
		assert!(effect(EffectKind::Numbered(14), routing(1)).matrix().is_none());
		assert_eq!(effect(EffectKind::Numbered(14), Vec::new()).name(), Some("CRIWARE/I3DL2Reverb"));
		assert_eq!(effect(EffectKind::Numbered(99), Vec::new()).name(), None);
	}

	#[test]
	fn category_commands() {
		let c = parse_commands(&[0x00, 0x49, 4, 0, 1, 0, 5, 0x00, 0x57, 2, 0, 0x47, 0, 0, 0]);
		let params: Vec<CueParam> =
			c.iter().map(|c| decode_param(c, CommandTable::Category, &[]).unwrap()).collect();
		assert_eq!(params[0], CueParam::CueLimit { instances: 1, slot: 5, kind: None });
		let category =
			Category { index: 0, id: 4, name: "SE".into(), group: 0, params, global_aisacs: Vec::new() };
		assert_eq!(category.volume(), Some(71.0 * 0.01));
	}
}
