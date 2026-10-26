// USM payload masks

#[derive(Debug, Clone)]
pub(crate) struct Masks {
	video1: [u8; 0x20],
	video2: [u8; 0x20],
	audio: [u8; 0x20],
}

const VIDEO_PLAIN_PREFIX: usize = 0x40;
const VIDEO_MIN_BODY: usize = 0x200;
const AUDIO_PLAIN_PREFIX: usize = 0x140;

impl Masks {
	pub(crate) fn derive(keycode: u64) -> Self {
		let k = keycode.to_le_bytes();
		let mut t = [0u8; 0x20];
		t[0x00] = k[0];
		t[0x01] = k[1];
		t[0x02] = k[2];
		t[0x03] = k[3].wrapping_sub(0x34);
		t[0x04] = k[4].wrapping_sub(0x07);
		t[0x05] = k[5] ^ 0x13;
		t[0x06] = k[6].wrapping_add(0x61);
		t[0x07] = !k[0];
		t[0x08] = k[1].wrapping_add(k[2]);
		t[0x09] = k[1].wrapping_add(k[0]).wrapping_add(1);
		t[0x0a] = !k[2];
		t[0x0b] = !k[1];
		t[0x0c] = t[0x09].wrapping_add(t[0x0b]);
		t[0x0d] = t[0x08].wrapping_sub(t[0x03]);
		t[0x0e] = !t[0x0d];
		t[0x0f] = t[0x0a].wrapping_add(k[1]).wrapping_add(1);
		t[0x10] = k[2].wrapping_mul(2);
		t[0x11] = t[0x07] ^ t[0x10];
		t[0x12] = !t[0x0f];
		t[0x13] = t[0x03] ^ 0x10;
		t[0x14] = k[4].wrapping_sub(0x39);
		t[0x15] = t[0x05].wrapping_sub(0x13);
		t[0x16] = t[0x06] ^ 0xf3;
		t[0x17] = t[0x13].wrapping_sub(t[0x0f]);
		t[0x18] = t[0x07].wrapping_add(t[0x15]);
		t[0x19] = 0x21u8.wrapping_sub(t[0x13]);
		t[0x1a] = t[0x14] ^ t[0x17];
		t[0x1b] = t[0x16].wrapping_mul(2);
		t[0x1c] = t[0x17].wrapping_add(0x44);
		t[0x1d] = t[0x03].wrapping_add(t[0x04]);
		t[0x1e] = t[0x05].wrapping_sub(t[0x16]);
		t[0x1f] = t[0x13] ^ t[0x1d];

		let video2 = t.map(|b| !b);
		let audio = std::array::from_fn(|i| if i & 1 != 0 { b"URUC"[(i >> 1) & 3] } else { video2[i] });
		Self { video1: t, video2, audio }
	}

	pub(crate) fn decrypt_video(&self, payload: &mut [u8]) {
		let Some(body) = payload.len().checked_sub(VIDEO_PLAIN_PREFIX) else { return };
		if body < VIDEO_MIN_BODY {
			return;
		}
		let d = &mut payload[VIDEO_PLAIN_PREFIX..];

		let mut roll = self.video2;
		for (i, b) in d[0x100..].iter_mut().enumerate() {
			let k = i & 0x1f;
			*b ^= roll[k];
			roll[k] = *b ^ self.video2[k];
		}

		let mut roll1 = self.video1;
		for blk in 0..8 {
			let off = blk * 0x20;
			for (r, b) in roll1.iter_mut().zip(&d[0x100 + off..0x120 + off]) {
				*r ^= b;
			}
			for (b, r) in d[off..off + 0x20].iter_mut().zip(&roll1) {
				*b ^= r;
			}
		}
	}

	pub(crate) fn decrypt_audio(&self, payload: &mut [u8]) {
		for (i, b) in payload.iter_mut().enumerate().skip(AUDIO_PLAIN_PREFIX) {
			*b ^= self.audio[i & 0x1f];
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn derived_masks() {
		let m = Masks::derive(crate::TEST_KEYCODE);
		assert_eq!(
			m.video1,
			[
				0xef, 0xcd, 0xab, 0x55, 0x60, 0x56, 0x84, 0x10, 0x78, 0xbd, 0x54, 0x32, 0xef, 0x23, 0xdc,
				0x22, 0x56, 0x46, 0xdd, 0x45, 0x2e, 0x43, 0x77, 0x23, 0x53, 0xdc, 0x0d, 0xee, 0x67, 0xb5,
				0xdf, 0xf0
			]
		);
		assert_eq!(m.video2, m.video1.map(|b| !b));
		assert_eq!(&m.audio[0..8], &[0x10, 0x55, 0x54, 0x52, 0x9f, 0x55, 0x7b, 0x43]);
	}

	#[test]
	fn video_round_trip() {
		let m = Masks::derive(crate::TEST_KEYCODE);
		let plain: Vec<u8> = (0..0x400u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();

		let mut enc = plain.clone();
		{
			let d = &mut enc[VIDEO_PLAIN_PREFIX..];
			let mut roll1 = m.video1;
			for blk in 0..8 {
				let off = blk * 0x20;
				for k in 0..0x20 {
					roll1[k] ^= d[0x100 + off + k];
				}
				for k in 0..0x20 {
					d[off + k] ^= roll1[k];
				}
			}
			let mut roll = m.video2;
			for (i, b) in d[0x100..].iter_mut().enumerate() {
				let k = i & 0x1f;
				let next = *b ^ m.video2[k];
				*b ^= roll[k];
				roll[k] = next;
			}
		}
		assert_ne!(enc, plain, "encryption left the payload untouched");

		let mut dec = enc;
		m.decrypt_video(&mut dec);
		assert_eq!(dec, plain);
	}

	#[test]
	fn short_payload() {
		let m = Masks::derive(crate::TEST_KEYCODE);
		let plain: Vec<u8> = (0..0x23fu16).map(|i| i as u8).collect();
		let mut d = plain.clone();
		m.decrypt_video(&mut d);
		assert_eq!(d, plain);
	}
}
