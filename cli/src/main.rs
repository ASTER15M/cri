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
	/// Show what a file holds.
	///
	/// An ACB's cues and waveforms, an AWB's entries, an ACF's categories, DSP bus settings, AISACs,
	/// REACT rules, selectors and voice limits, or a USM's streams. With no section named, every
	/// section.
	Info {
		file: PathBuf,
		/// Only the cues with one of these IDs or whose names contain one, ignoring case, and their
		/// waveforms. ACB only.
		patterns: Vec<String>,
		/// The cues, one line each. ACB only.
		#[arg(long)]
		cues: bool,
		/// The waveforms, beside each bitstream's own header. ACB or AWB.
		#[arg(long)]
		waves: bool,
		/// Each cue's program in full: parameters, tracks, layers, AISACs and action tracks. ACB
		/// only; implies --cues.
		#[arg(long)]
		full: bool,
		/// AISAC graph points too. ACB or ACF; for an ACB, implies --full.
		#[arg(long)]
		graphs: bool,
		#[command(flatten)]
		awb: AwbArg,
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
	/// Extract audio and video.
	///
	/// An ACB's or AWB's waveforms, decoded to WAV or as stored; a USM's video, alpha and audio
	/// streams, its audio decoded unless --raw; or an HCA or ADX file, decoded to WAV.
	Extract {
		/// An ACB, whose files are named by the first cue that plays them, an AWB, a USM, or an HCA or
		/// ADX file.
		input: PathBuf,
		/// Only the waveforms of the cues with one of these IDs or whose names contain one, ignoring
		/// case. ACB only.
		patterns: Vec<String>,
		/// The directory to write into; created when missing. Defaults to a directory beside the
		/// input, named after it, or for an HCA or ADX file, the input's own directory.
		#[arg(short, long)]
		out: Option<PathBuf>,
		/// Write the HCA and ADX bitstreams as stored instead of decoding them.
		#[arg(long)]
		raw: bool,
		/// The AWB subkey a loose HCA or ADX file was stored under, for keyed HCA and ADX type 9.
		#[arg(long)]
		subkey: Option<u16>,
		#[command(flatten)]
		awb: AwbArg,
		/// The keycode for encrypted HCA, ADX and USM, in decimal or `0x` hex.
		#[arg(long, env = "CRI_KEY", value_parser = parse_key, hide_env_values = true)]
		key: Option<u64>,
		/// Write 16-bit PCM instead of 32-bit float.
		#[arg(long)]
		pcm16: bool,
	},
}

#[derive(clap::Args)]
struct AwbArg {
	/// The ACB's streaming AWB; defaults to the `.awb` beside it, when there is one.
	#[arg(long)]
	awb: Option<PathBuf>,
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
		Cmd::Info { file, patterns, cues, waves, full, graphs, awb } => {
			let o = explore::Info { patterns, cues, waves, full, graphs, awb: awb.awb };
			explore::info(&file, o)
		}
		Cmd::Utf { file, table, schema, columns, row, limit } => {
			explore::utf(&file, table.as_deref(), schema, &columns, row, limit)
		}
		Cmd::Extract { input, patterns, out, raw, subkey, awb, key, pcm16 } => {
			let o = write::Extract { patterns, out, raw, subkey, awb: awb.awb, keycode: key, pcm16 };
			write::extract(&input, o)
		}
	}
}
