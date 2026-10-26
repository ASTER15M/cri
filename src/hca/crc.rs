// The block CRC-16, stored in the block's last two bytes and checked over the ciphertext

const TABLE: [u16; 256] = {
	let mut t = [0u16; 256];
	let mut i = 0usize;
	while i < 256 {
		let mut c = (i as u16) << 8;
		let mut b = 0;
		while b < 8 {
			c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
			b += 1;
		}
		t[i] = c;
		i += 1;
	}
	t
};

fn crc16(data: &[u8]) -> u16 {
	let mut crc: u16 = 0;
	for b in data {
		crc = (crc << 8) ^ TABLE[(*b as usize) ^ ((crc >> 8) as usize)];
	}
	crc
}

pub fn block_is_intact(block: &[u8]) -> bool {
	crc16(block) == 0
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn known_vector() {
		// CRC-16/UMTS ("BUYPASS") check value for b"123456789".
		assert_eq!(crc16(b"123456789"), 0xfee8);
	}

	#[test]
	fn block_is_intact_with_crc() {
		let payload = b"the quick brown fox";
		let mut framed = payload.to_vec();
		framed.extend_from_slice(&crc16(payload).to_be_bytes());
		assert!(block_is_intact(&framed));
	}
}
