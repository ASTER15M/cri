// AFS2 (`.awb`) archives

use crate::error::{Error, Format, Result, ensure};

#[derive(Debug, Clone, Copy)]
pub struct Afs2Entry {
	pub id: u32,
	pub offset: usize, // into the buffer the archive was parsed from
	pub len: usize,
}

#[derive(Debug, Clone)]
pub struct Afs2 {
	pub align: u16,
	pub subkey: u16, // mixed into the HCA and ADX keycode when non-zero
	pub entries: Vec<Afs2Entry>,
}

impl Afs2 {
	pub fn parse(buf: &[u8]) -> Result<Self> {
		let hdr = buf.get(..16).ok_or(Error::truncated(Format::Afs2, 16, buf.len()))?;
		ensure!(&hdr[0..4] == b"AFS2", Error::malformed(Format::Afs2, "bad magic"));

		let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
		let offset_size = ((version >> 8) & 0xff) as usize;
		let id_size = ((version >> 16) & 0xff) as usize;
		let count = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
		let align = u16::from_le_bytes(hdr[12..14].try_into().unwrap());
		let subkey = u16::from_le_bytes(hdr[14..16].try_into().unwrap());

		if !matches!(offset_size, 2 | 4 | 8) || !matches!(id_size, 2 | 4) {
			let feature = format!("offset size {offset_size} with id size {id_size}");
			return Err(Error::unsupported(Format::Afs2, feature));
		}
		ensure!(align != 0, Error::malformed(Format::Afs2, "zero alignment"));

		let read = |p: usize, n: usize| -> Result<u64> {
			let s = buf.get(p..p + n).ok_or(Error::truncated(Format::Afs2, p + n, buf.len()))?;
			Ok(s.iter().rev().fold(0, |v, &b| (v << 8) | b as u64))
		};

		let mut p = 16;
		let mut ids = Vec::with_capacity(count);
		for _ in 0..count {
			ids.push(read(p, id_size)? as u32);
			p += id_size;
		}
		let mut offs = Vec::with_capacity(count + 1);
		for _ in 0..=count {
			offs.push(read(p, offset_size)? as usize);
			p += offset_size;
		}

		let a = align as usize;
		let mut entries = Vec::with_capacity(count);
		for i in 0..count {
			// Starts round up to `align`; ends do not.
			let start = offs[i].div_ceil(a) * a;
			let end = offs[i + 1];
			ensure!(end <= buf.len(), Error::truncated(Format::Afs2, end, buf.len()));
			let reason = || format!("entry {i} ends at {end:#x}, before its start {start:#x}");
			ensure!(end >= start, Error::malformed(Format::Afs2, reason()));
			entries.push(Afs2Entry { id: ids[i], offset: start, len: end - start });
		}

		Ok(Self { align, subkey, entries })
	}
}
