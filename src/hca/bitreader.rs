// MSB-first bit reader over an HCA block. Reads past the end yield zero bits: the decoder peeks up
// to 12 bits near the end of a block.

pub(crate) struct BitReader<'a> {
	data: &'a [u8],
	pos: usize,
}

impl<'a> BitReader<'a> {
	pub(crate) fn new(data: &'a [u8]) -> Self {
		Self { data, pos: 0 }
	}

	// `n` bits, at most 32, without consuming them
	pub(crate) fn peek(&self, n: u32) -> u32 {
		if n == 0 {
			return 0;
		}
		let byte = self.pos >> 3;
		let bit = (self.pos & 7) as u32;
		let mut acc: u64 = 0;
		for k in 0..5 {
			acc = (acc << 8) | *self.data.get(byte + k).unwrap_or(&0) as u64;
		}
		((acc >> (40 - bit - n)) & ((1u64 << n) - 1)) as u32
	}
	pub(crate) fn skip(&mut self, n: u32) {
		self.pos += n as usize;
	}
	pub(crate) fn read(&mut self, n: u32) -> u32 {
		let v = self.peek(n);
		self.skip(n);
		v
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn msb_first() {
		// 1010 1010 0101 0101 1111 0000
		let data = [0xaa, 0x55, 0xf0];
		let mut br = BitReader::new(&data);
		assert_eq!(br.read(4), 0b1010);
		assert_eq!(br.read(8), 0b1010_0101);
		assert_eq!(br.read(12), 0b0101_1111_0000);
		assert_eq!(br.read(8), 0);
	}

	#[test]
	fn past_the_end() {
		let data = [0xff];
		let mut br = BitReader::new(&data);
		assert_eq!(br.read(8), 0xff);
		assert_eq!(br.read(8), 0);
	}

	#[test]
	fn peek() {
		let data = [0b1101_0000];
		let mut br = BitReader::new(&data);
		assert_eq!(br.peek(3), 0b110);
		assert_eq!(br.peek(3), 0b110);
		assert_eq!(br.read(4), 0b1101);
	}
}
