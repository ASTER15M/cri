// `cri`: list, explore and decode CRI files from the command line

mod explore;
mod write;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cri", version, about = "List, explore and decode CRI Middleware files")]
struct Args {
	#[command(subcommand)]
	cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// List CRI files.
	///
	/// One line per file. A directory lists the files in it that read as CRI files: ACB, ACF, AWB,
	/// USM, HCA, ADX or a bare @UTF table.
	Ls {
		/// Files and directories; defaults to the current directory.
		paths: Vec<PathBuf>,
		/// List the directories under each directory too.
		#[arg(short, long)]
		recursive: bool,
		/// Several lines per file: versions, sizes, codecs and archives.
		#[arg(short, long)]
		long: bool,
	},
	/// Browse @UTF tables.
	///
	/// The @UTF tables of an ACB or ACF: the root's tables, or the rows of one of them.
	Utf {
		file: PathBuf,
		/// A table in the root, such as `CueTable`. `A/B` reads table `B` out of `A`'s first row.
		table: Option<String>,
		/// Print the columns with their types instead of the rows.
		#[arg(long)]
		schema: bool,
		/// Only these columns, one row per line. Comma-separated.
		#[arg(short, long, value_delimiter = ',')]
		columns: Vec<String>,
		/// Only this row.
		#[arg(long)]
		row: Option<usize>,
		/// At most this many rows; 0 prints all of them.
		#[arg(short, long, default_value_t = 20)]
		limit: usize,
	},
	/// List an ACB's cues.
	///
	/// One line per cue: id, name, length, sequence type, layers and categories.
	Cues {
		acb: PathBuf,
		/// Only cues whose name contains this, ignoring case.
		filter: Option<String>,
		#[command(flatten)]
		awb: AwbArg,
	},
	/// Show a cue's program.
	///
	/// A cue's program in full: every scope's parameters, tracks, layers, AISACs and action tracks.
	Cue {
		acb: PathBuf,
		/// Cue names or numeric cue IDs.
		#[arg(required = true)]
		cues: Vec<String>,
		/// Also print each AISAC's graph points.
		#[arg(long)]
		graphs: bool,
		#[command(flatten)]
		awb: AwbArg,
	},
	/// List an ACB's waveforms.
	///
	/// An ACB's waveforms, with the header of each bitstream beside what the ACB says.
	Waves {
		acb: PathBuf,
		#[command(flatten)]
		awb: AwbArg,
	},
	/// List an AWB's entries.
	///
	/// An AWB's entries, with each bitstream's header.
	Awb { awb: PathBuf },
	/// Show an ACF's settings.
	///
	/// An ACF's categories, DSP bus settings, AISACs, REACT rules, selectors and voice limits.
	Acf {
		acf: PathBuf,
		/// Also print each global AISAC's graph points.
		#[arg(long)]
		graphs: bool,
	},
	/// Show a USM's streams.
	///
	/// A USM's streams, video format and chunks.
	Usm { usm: PathBuf },
	/// Decode HCA or ADX to WAV.
	///
	/// Decode an HCA or ADX file to WAV.
	Decode {
		input: PathBuf,
		/// The WAV to write; defaults to the input with a `.wav` extension.
		#[arg(short, long)]
		out: Option<PathBuf>,
		/// The AWB subkey the file was stored under, for keyed HCA and ADX type 9.
		#[arg(long, default_value_t = 0)]
		subkey: u16,
		#[command(flatten)]
		key: KeyArg,
		#[command(flatten)]
		format: FormatArg,
	},
	/// Extract an ACB's or AWB's waveforms.
	///
	/// Write every waveform of an ACB or AWB to a directory, decoded to WAV or as stored.
	Extract {
		/// An ACB, whose files are named by the first cue that plays them, or an AWB.
		input: PathBuf,
		/// The directory to write into; created when missing.
		#[arg(short, long)]
		out: PathBuf,
		/// Write the HCA or ADX bitstreams as stored instead of decoding them.
		#[arg(long)]
		raw: bool,
		/// Only waveforms of cues whose name contains this, ignoring case. ACB only.
		#[arg(long)]
		cue: Option<String>,
		#[command(flatten)]
		awb: AwbArg,
		#[command(flatten)]
		key: KeyArg,
		#[command(flatten)]
		format: FormatArg,
	},
	/// Split a USM into streams.
	///
	/// Split a USM into its video, audio and alpha streams.
	Demux {
		usm: PathBuf,
		/// The directory to write into; created when missing.
		#[arg(short, long)]
		out: PathBuf,
		#[command(flatten)]
		key: KeyArg,
	},
}

#[derive(clap::Args)]
struct AwbArg {
	/// The ACB's streaming AWB; defaults to the `.awb` beside it, when there is one.
	#[arg(long)]
	awb: Option<PathBuf>,
}

#[derive(clap::Args)]
struct KeyArg {
	/// The keycode for encrypted HCA, ADX and USM, in decimal or `0x` hex.
	#[arg(long, env = "CRI_KEY", value_parser = parse_key, hide_env_values = true)]
	key: Option<u64>,
}

#[derive(clap::Args)]
struct FormatArg {
	/// Write 16-bit PCM instead of 32-bit float.
	#[arg(long)]
	pcm16: bool,
}

fn parse_key(s: &str) -> Result<u64> {
	let s = s.trim();
	match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
		Some(hex) => u64::from_str_radix(hex, 16).context("not a hex keycode"),
		None => s.parse().context("not a decimal keycode"),
	}
}

fn main() -> Result<()> {
	// Output piped into `head` stops being read; that ends the listing, not with a panic.
	let default = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		if info.payload_as_str().is_some_and(|m| m.starts_with("failed printing to stdout")) {
			std::process::exit(0);
		}
		default(info);
	}));
	match Args::parse().cmd {
		Cmd::Ls { paths, recursive, long } => explore::ls(&paths, recursive, long),
		Cmd::Utf { file, table, schema, columns, row, limit } => {
			explore::utf(&file, table.as_deref(), schema, &columns, row, limit)
		}
		Cmd::Cues { acb, filter, awb } => explore::cues(&acb, awb.awb, filter.as_deref()),
		Cmd::Cue { acb, cues, graphs, awb } => explore::cue(&acb, awb.awb, &cues, graphs),
		Cmd::Waves { acb, awb } => explore::waves(&acb, awb.awb),
		Cmd::Awb { awb } => explore::awb(&awb),
		Cmd::Acf { acf, graphs } => explore::acf(&acf, graphs),
		Cmd::Usm { usm } => explore::usm(&usm),
		Cmd::Decode { input, out, subkey, key, format } => {
			let out = out.unwrap_or_else(|| input.with_extension("wav"));
			write::decode(&input, &out, key.key, subkey, format.pcm16)
		}
		Cmd::Extract { input, out, raw, cue, awb, key, format } => {
			let opts = write::Extract { raw, cue, keycode: key.key, pcm16: format.pcm16 };
			write::extract(&input, awb.awb, &out, &opts)
		}
		Cmd::Demux { usm, out, key } => write::demux(&usm, &out, key.key),
	}
}
