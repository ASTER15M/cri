// HCA block cipher (`ciph` types 0, 1 and 56), a byte substitution. Every table maps 0x00 and
// 0xFF to themselves, so the sync marker cannot validate a key.

use crate::error::{Error, Format, Result, ensure};

#[derive(Clone)]
pub struct CipherTable(pub [u8; 256]);

impl CipherTable {
	fn new(table: [u8; 256]) -> Result<Self> {
		ensure!(
			table[0] == 0 && table[255] == 0xff,
			Error::malformed(Format::Hca, "the cipher table moves 0x00 or 0xFF")
		);
		Ok(Self(table))
	}

	// The table for a `ciph` type. `subkey`, from the AFS2 header, is mixed into the keycode when
	// non-zero.
	pub fn for_type(ciph_type: u16, keycode: u64, subkey: u16) -> Result<Option<Self>> {
		match ciph_type {
			0 => Ok(None),
			1 => Ok(Some(Self::new(init_type1())?)),
			56 => {
				ensure!(keycode != 0, Error::MissingKeycode { format: Format::Hca });
				let kc = mix_subkey(keycode, subkey);
				Ok(Some(Self::new(init_type56(kc))?))
			}
			other => Err(Error::unsupported(Format::Hca, format!("cipher type {other}"))),
		}
	}

	pub fn apply(&self, block: &mut [u8]) {
		for b in block.iter_mut() {
			*b = self.0[*b as usize];
		}
	}
}

fn mix_subkey(keycode: u64, subkey: u16) -> u64 {
	if subkey == 0 {
		keycode
	} else {
		keycode.wrapping_mul(((subkey as u64) << 16) | ((!subkey as u64 + 2) & 0xffff))
	}
}

fn init_type1() -> [u8; 256] {
	let mut t = [0u8; 256];
	let mut v: u32 = 0;
	for slot in t.iter_mut().take(0xff).skip(1) {
		v = (v.wrapping_mul(13).wrapping_add(11)) & 0xff;
		if v == 0 || v == 0xff {
			v = (v.wrapping_mul(13).wrapping_add(11)) & 0xff;
		}
		*slot = v as u8;
	}
	t[0] = 0;
	t[0xff] = 0xff;
	t
}

fn sub_table(key: u8) -> [u8; 16] {
	let mul = ((key as u32 & 1) << 3) | 5;
	let add = (key as u32 & 0xe) | 1;
	let mut k = (key >> 4) as u32;
	let mut r = [0u8; 16];
	for slot in r.iter_mut() {
		k = (k.wrapping_mul(mul).wrapping_add(add)) & 0xf;
		*slot = k as u8;
	}
	r
}

fn init_type56(keycode: u64) -> [u8; 256] {
	// The low 56 bits, decremented; zero stays zero.
	let mut k = keycode.saturating_sub(1);
	let mut kc = [0u8; 7];
	for slot in kc.iter_mut() {
		*slot = (k & 0xff) as u8;
		k >>= 8;
	}

	let seed = [
		kc[1],
		kc[1] ^ kc[6],
		kc[2] ^ kc[3],
		kc[2],
		kc[2] ^ kc[1],
		kc[3] ^ kc[4],
		kc[3],
		kc[3] ^ kc[2],
		kc[4] ^ kc[5],
		kc[4],
		kc[4] ^ kc[3],
		kc[5] ^ kc[6],
		kc[5],
		kc[5] ^ kc[4],
		kc[6] ^ kc[1],
		kc[6],
	];

	let mut base = [0u8; 256];
	let base_r = sub_table(kc[0]);
	for r in 0..16 {
		let base_c = sub_table(seed[r]);
		let nb = base_r[r] << 4;
		for c in 0..16 {
			base[r * 16 + c] = nb | base_c[c];
		}
	}

	// Steps of 17 (coprime with 256) visit every slot once; the two reserved values are skipped.
	let mut table = [0u8; 256];
	let mut x: usize = 0;
	let mut out = 1usize;
	for _ in 0..256 {
		x = (x + 17) & 0xff;
		let v = base[x];
		if v != 0 && v != 0xff {
			table[out] = v;
			out += 1;
		}
	}
	table[0] = 0;
	table[0xff] = 0xff;
	table
}

#[cfg(test)]
mod tests {
	use super::*;

	// A derived table keeps 0x00 and 0xFF in place and is a permutation.
	#[test]
	fn type56_table() {
		for kc in [crate::TEST_KEYCODE, 1, 0xdead_beef_cafe] {
			let t = CipherTable::for_type(56, kc, 0).unwrap().unwrap();
			let mut seen = [false; 256];
			for b in t.0 {
				assert!(!seen[b as usize], "keycode {kc} produced a duplicate entry {b}");
				seen[b as usize] = true;
			}
			assert_eq!(t.0[0], 0);
			assert_eq!(t.0[255], 0xff);
		}
	}

	#[test]
	fn type1_table() {
		let t = CipherTable::for_type(1, 0, 0).unwrap().unwrap();
		let mut seen = [false; 256];
		for b in t.0 {
			assert!(!seen[b as usize], "duplicate entry {b}");
			seen[b as usize] = true;
		}
	}

	#[test]
	fn type0_plaintext() {
		assert!(CipherTable::for_type(0, 0, 0).unwrap().is_none());
	}
}
