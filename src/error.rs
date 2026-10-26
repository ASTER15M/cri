// The error every fallible function in this crate returns

use std::fmt;
use std::path::PathBuf;

use crate::acb::program::CommandTable;

pub type Result<T, E = Error> = std::result::Result<T, E>;

// The format whose input an error is about
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
	Utf,
	Acb,
	Acf,
	Afs2,
	Adx,
	Hca,
	Usm,
	Ivf,
	Vp9,
}

impl fmt::Display for Format {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Utf => "@UTF",
			Self::Acb => "ACB",
			Self::Acf => "ACF",
			Self::Afs2 => "AFS2",
			Self::Adx => "ADX",
			Self::Hca => "HCA",
			Self::Usm => "USM",
			Self::Ivf => "IVF",
			Self::Vp9 => "VP9",
		})
	}
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
	// The input ends before byte `needed` of something it declares
	Truncated { format: Format, needed: usize, len: usize },
	// The input breaks the format: a bad magic, or fields that contradict each other
	Malformed { format: Format, reason: String },
	// Valid input that this crate does not decode
	Unsupported { format: Format, feature: String },
	// A cue reaches a command this crate does not model. Skipping a parameter would change how the
	// cue sounds, and skipping a sequencer command when it plays.
	UnsupportedCommand { table: CommandTable, code: u16, payload_len: usize },
	// Encrypted input with no keycode, or with a zero one
	MissingKeycode { format: Format },
	// An HCA block whose CRC-16 does not match
	ChecksumMismatch,
	// No cue has this name or id
	NoSuchCue(String),
	// Building the program of cue `index` failed
	Cue { index: usize, name: String, source: Box<Error> },
	Io { path: PathBuf, source: std::io::Error },
}

impl Error {
	pub(crate) fn truncated(format: Format, needed: usize, len: usize) -> Self {
		Self::Truncated { format, needed, len }
	}
	pub(crate) fn malformed(format: Format, reason: impl Into<String>) -> Self {
		Self::Malformed { format, reason: reason.into() }
	}
	pub(crate) fn unsupported(format: Format, feature: impl Into<String>) -> Self {
		Self::Unsupported { format, feature: feature.into() }
	}

	// The error under any `Cue` wrapping
	pub fn root(&self) -> &Self {
		match self {
			Self::Cue { source, .. } => source.root(),
			other => other,
		}
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Truncated { format, needed, len } => {
				write!(f, "{format}: truncated: needs {needed} bytes, has {len}")
			}
			Self::Malformed { format, reason } => write!(f, "{format}: {reason}"),
			Self::Unsupported { format, feature } => write!(f, "{format}: {feature} is not supported"),
			Self::UnsupportedCommand { table, code, payload_len } => {
				write!(f, "{table}: command {code} ({payload_len} byte payload) is not supported")
			}
			Self::MissingKeycode { format } => write!(f, "{format}: encrypted, and no keycode was given"),
			Self::ChecksumMismatch => f.write_str("HCA: block failed its CRC"),
			Self::NoSuchCue(cue) => write!(f, "{cue}: no such cue"),
			Self::Cue { index, name, .. } if name.is_empty() => write!(f, "cue [{index}]"),
			Self::Cue { index, name, .. } => write!(f, "cue [{index}] {name}"),
			Self::Io { path, .. } => write!(f, "reading {}", path.display()),
		}
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Cue { source, .. } => Some(source.as_ref()),
			Self::Io { source, .. } => Some(source),
			_ => None,
		}
	}
}

// Returns `err` unless `cond` holds
macro_rules! ensure {
	($cond:expr, $err:expr $(,)?) => {
		if !($cond) {
			return Err($err);
		}
	};
}
pub(crate) use ensure;
