// `@UTF` tables, big-endian. A column's storage class decides where its value lives: nowhere
// (zero), once in the schema (constant) or in every row. Only per-row columns occupy row bytes.

use crate::error::{Error, Format, Result, ensure};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
	U8(u8),
	I8(i8),
	U16(u16),
	I16(i16),
	U32(u32),
	I32(i32),
	U64(u64),
	I64(i64),
	F32(f32),
	F64(f64),
	Str(String),
	Data(Vec<u8>), // usually a nested `@UTF` table or a command stream
}

impl Value {
	pub(crate) fn as_i64(&self) -> Option<i64> {
		Some(match self {
			Self::U8(v) => *v as i64,
			Self::I8(v) => *v as i64,
			Self::U16(v) => *v as i64,
			Self::I16(v) => *v as i64,
			Self::U32(v) => *v as i64,
			Self::I32(v) => *v as i64,
			Self::U64(v) => *v as i64,
			Self::I64(v) => *v,
			_ => return None,
		})
	}
	pub fn as_str(&self) -> Option<&str> {
		if let Self::Str(s) = self { Some(s) } else { None }
	}
	fn as_bytes(&self) -> Option<&[u8]> {
		if let Self::Data(d) = self { Some(d) } else { None }
	}
}

impl std::fmt::Display for Value {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Str(s) => write!(f, "{s:?}"),
			Self::Data(d) if d.is_empty() => write!(f, "<empty>"),
			Self::Data(d) if d.starts_with(b"@UTF") => write!(f, "<@UTF {} bytes>", d.len()),
			Self::Data(d) => {
				write!(f, "<{} bytes ", d.len())?;
				for b in d.iter().take(24) {
					write!(f, "{b:02x}")?;
				}
				if d.len() > 24 {
					write!(f, "..")?;
				}
				write!(f, ">")
			}
			Self::F32(x) => write!(f, "{x}"),
			Self::F64(x) => write!(f, "{x}"),
			v => write!(f, "{}", v.as_i64().unwrap()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
	Zero,
	Constant,
	PerRow,
}

#[derive(Debug, Clone)]
pub struct Column {
	pub name: String,
	pub type_id: u8,
	storage: Storage,
	constant: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Utf {
	pub name: String,
	pub columns: Vec<Column>,
	pub rows: Vec<Vec<Value>>,
}

impl Utf {
	pub fn parse(buf: &[u8]) -> Result<Self> {
		let hdr = buf.get(..32).ok_or(Error::truncated(Format::Utf, 32, buf.len()))?;
		ensure!(&hdr[0..4] == b"@UTF", Error::malformed(Format::Utf, "bad magic"));

		// Offsets are relative to `base`, past the 8-byte magic and size.
		let base = 8;
		let bytes = |p: usize, n: usize| -> Result<&[u8]> {
			buf.get(p..p + n).ok_or(Error::truncated(Format::Utf, p + n, buf.len()))
		};
		let be16 = |p| -> Result<u16> { Ok(u16::from_be_bytes(bytes(p, 2)?.try_into().unwrap())) };
		let be32 = |p| -> Result<u32> { Ok(u32::from_be_bytes(bytes(p, 4)?.try_into().unwrap())) };

		let rows_off = base + be16(base + 2)? as usize;
		let str_off = base + be32(base + 4)? as usize;
		let data_off = base + be32(base + 8)? as usize;
		let name_off = be32(base + 12)? as usize;
		let n_cols = be16(base + 16)? as usize;
		let row_width = be16(base + 18)? as usize;
		let n_rows = be32(base + 20)? as usize;

		let read_str = |o: usize| -> String {
			let s = str_off + o;
			let end = buf[s..].iter().position(|&b| b == 0).map_or(buf.len(), |n| s + n);
			String::from_utf8_lossy(&buf[s..end]).into_owned()
		};

		let read_val = |type_id: u8, p: &mut usize| -> Result<Value> {
			let v = match type_id {
				0x00 => Value::I8(buf[*p] as i8),
				0x01 => Value::U8(buf[*p]),
				0x02 => Value::I16(be16(*p)? as i16),
				0x03 => Value::U16(be16(*p)?),
				0x04 => Value::I32(be32(*p)? as i32),
				0x05 => Value::U32(be32(*p)?),
				0x06 | 0x07 => {
					let raw = ((be32(*p)? as u64) << 32) | be32(*p + 4)? as u64;
					if type_id == 0x06 { Value::I64(raw as i64) } else { Value::U64(raw) }
				}
				0x08 => Value::F32(f32::from_bits(be32(*p)?)),
				0x09 => Value::F64(f64::from_bits(((be32(*p)? as u64) << 32) | be32(*p + 4)? as u64)),
				0x0a => Value::Str(read_str(be32(*p)? as usize)),
				0x0b => {
					let s = data_off + be32(*p)? as usize;
					let n = be32(*p + 4)? as usize;
					let slice = buf.get(s..s + n).ok_or(Error::truncated(Format::Utf, s + n, buf.len()))?;
					Value::Data(slice.to_vec())
				}
				other => {
					return Err(Error::unsupported(Format::Utf, format!("column type {other:#04x}")));
				}
			};
			*p += match type_id {
				0x00 | 0x01 => 1,
				0x02 | 0x03 => 2,
				0x04 | 0x05 | 0x08 | 0x0a => 4,
				_ => 8,
			};
			Ok(v)
		};

		let mut p = base + 24;
		let mut columns = Vec::with_capacity(n_cols);
		for _ in 0..n_cols {
			let flags = buf[p];
			let name = read_str(be32(p + 1)? as usize);
			p += 5;
			let type_id = flags & 0x0f;
			let (storage, constant) = match flags & 0xf0 {
				0x10 => (Storage::Zero, None),
				0x30 => (Storage::Constant, Some(read_val(type_id, &mut p)?)),
				_ => (Storage::PerRow, None),
			};
			columns.push(Column { name, type_id, storage, constant });
		}

		let zero_of = |type_id: u8| match type_id {
			0x00 => Value::I8(0),
			0x01 => Value::U8(0),
			0x02 => Value::I16(0),
			0x03 => Value::U16(0),
			0x04 => Value::I32(0),
			0x05 => Value::U32(0),
			0x06 => Value::I64(0),
			0x07 => Value::U64(0),
			0x08 => Value::F32(0.0),
			0x09 => Value::F64(0.0),
			0x0a => Value::Str(String::new()),
			_ => Value::Data(Vec::new()),
		};

		let mut rows = Vec::with_capacity(n_rows);
		for r in 0..n_rows {
			let mut p = rows_off + r * row_width;
			let mut row = Vec::with_capacity(n_cols);
			for c in &columns {
				row.push(match c.storage {
					Storage::Zero => zero_of(c.type_id),
					Storage::Constant => c.constant.clone().unwrap(),
					Storage::PerRow => read_val(c.type_id, &mut p)?,
				});
			}
			rows.push(row);
		}

		Ok(Self { name: read_str(name_off), columns, rows })
	}

	pub(crate) fn empty(name: &str) -> Self {
		Self { name: name.into(), columns: Vec::new(), rows: Vec::new() }
	}

	pub fn get(&self, row: usize, name: &str) -> Option<&Value> {
		let c = self.columns.iter().position(|c| c.name == name)?;
		self.rows.get(row)?.get(c)
	}
	pub(crate) fn get_i64(&self, row: usize, name: &str) -> Option<i64> {
		self.get(row, name)?.as_i64()
	}
	pub(crate) fn get_str(&self, row: usize, name: &str) -> Option<&str> {
		self.get(row, name)?.as_str()
	}
	pub fn get_bytes(&self, row: usize, name: &str) -> Option<&[u8]> {
		self.get(row, name)?.as_bytes()
	}
	// A cell holding a nested table; `None` when it is empty, as a table with no rows is
	pub(crate) fn get_table(&self, row: usize, name: &str) -> Option<Result<Self>> {
		let d = self.get_bytes(row, name)?;
		(!d.is_empty()).then(|| Self::parse(d))
	}
	// A cell of big-endian `u16` indexes into another table
	pub(crate) fn get_indexes(&self, row: usize, name: &str) -> Vec<usize> {
		let b = self.get_bytes(row, name).unwrap_or_default();
		b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c) as usize).collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// One column of each storage class: only per-row columns consume row bytes.
	#[test]
	fn storage_classes() {
		let mut b = Vec::new();
		b.extend_from_slice(b"@UTF");
		b.extend_from_slice(&0u32.to_be_bytes()); // size, unread
		let strings = b"table\0zero\0konst\0perrow\0";
		// header: version, rows_off, str_off, data_off, name_off, n_cols, row_width, n_rows
		let cols_len = (1 + 4) + (1 + 4 + 4) + (1 + 4); // zero, constant(+u32), per-row
		let rows_off = 24 + cols_len;
		b.extend_from_slice(&1u16.to_be_bytes());
		b.extend_from_slice(&(rows_off as u16).to_be_bytes());
		b.extend_from_slice(&((rows_off + 8) as u32).to_be_bytes()); // str_off
		b.extend_from_slice(&((rows_off + 8 + strings.len()) as u32).to_be_bytes()); // data_off
		b.extend_from_slice(&0u32.to_be_bytes()); // name_off -> "table"
		b.extend_from_slice(&3u16.to_be_bytes());
		b.extend_from_slice(&4u16.to_be_bytes()); // only the per-row u32
		b.extend_from_slice(&2u32.to_be_bytes());
		b.extend_from_slice(&[0x15]); // zero storage, u32
		b.extend_from_slice(&6u32.to_be_bytes()); // "zero"
		b.extend_from_slice(&[0x35]); // constant storage, u32
		b.extend_from_slice(&11u32.to_be_bytes()); // "konst"
		b.extend_from_slice(&0xdead_beefu32.to_be_bytes());
		b.extend_from_slice(&[0x55]); // per-row, u32
		b.extend_from_slice(&17u32.to_be_bytes()); // "perrow"
		b.extend_from_slice(&111u32.to_be_bytes());
		b.extend_from_slice(&222u32.to_be_bytes());
		b.extend_from_slice(strings);

		let t = Utf::parse(&b).unwrap();
		assert_eq!(t.name, "table");
		assert_eq!(t.rows.len(), 2);
		assert_eq!(t.get_i64(0, "zero"), Some(0));
		assert_eq!(t.get_i64(1, "zero"), Some(0));
		assert_eq!(t.get_i64(0, "konst"), Some(0xdead_beef));
		assert_eq!(t.get_i64(1, "konst"), Some(0xdead_beef));
		assert_eq!(t.get_i64(0, "perrow"), Some(111));
		assert_eq!(t.get_i64(1, "perrow"), Some(222));
	}

	#[test]
	fn foreign_buffer() {
		assert!(Utf::parse(b"AFS2\0\0\0\0................................").is_err());
	}
}
