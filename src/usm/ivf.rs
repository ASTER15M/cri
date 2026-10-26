// IVF framing and VP9 packet headers

use crate::error::{Error, Format, Result, ensure};

const FILE_HEADER_LEN: usize = 32;
const FRAME_HEADER_LEN: usize = 12;

#[derive(Debug, Clone, Copy)]
pub struct Frame {
	pub offset: usize,
	pub len: usize,
	pub pts: u64,
}

#[derive(Debug, Clone)]
pub struct Ivf {
	pub fourcc: [u8; 4],
	pub width: u16,
	pub height: u16,
	pub rate: u32,
	pub scale: u32,
	pub declared_frames: u32,
	pub frames: Vec<Frame>,
}

impl Ivf {
	pub fn parse(buf: &[u8]) -> Result<Self> {
		ensure!(
			buf.len() >= FILE_HEADER_LEN,
			Error::truncated(Format::Ivf, FILE_HEADER_LEN, buf.len())
		);
		ensure!(&buf[0..4] == b"DKIF", Error::malformed(Format::Ivf, "bad magic"));
		let hdr_len = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
		let short = || Error::malformed(Format::Ivf, format!("header length {hdr_len} is below 32"));
		ensure!(hdr_len >= FILE_HEADER_LEN, short());
		ensure!(hdr_len <= buf.len(), Error::truncated(Format::Ivf, hdr_len, buf.len()));

		let mut frames = Vec::new();
		let mut off = hdr_len;
		while off < buf.len() {
			ensure!(
				off + FRAME_HEADER_LEN <= buf.len(),
				Error::truncated(Format::Ivf, off + FRAME_HEADER_LEN, buf.len())
			);
			let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
			let pts = u64::from_le_bytes(buf[off + 4..off + 12].try_into().unwrap());
			let start = off + FRAME_HEADER_LEN;
			ensure!(start + len <= buf.len(), Error::truncated(Format::Ivf, start + len, buf.len()));
			frames.push(Frame { offset: start, len, pts });
			off = start + len;
		}
		Ok(Self {
			fourcc: buf[8..12].try_into().unwrap(),
			width: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
			height: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
			rate: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
			scale: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
			declared_frames: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
			frames,
		})
	}

	pub fn fps(&self) -> f64 {
		if self.scale == 0 { 0.0 } else { self.rate as f64 / self.scale as f64 }
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9FrameHead {
	pub profile: u8,
	pub show_existing_frame: bool,
	pub keyframe: bool,
	pub show_frame: bool,
	pub error_resilient: bool,
}

pub fn peek_vp9(frame: &[u8]) -> Result<Vp9FrameHead> {
	ensure!(!frame.is_empty(), Error::truncated(Format::Vp9, 1, 0));
	let b = frame[0];
	let marker =
		|| Error::malformed(Format::Vp9, format!("frame marker is {:#04b}, not 0b10", b >> 6));
	ensure!(b >> 6 == 0b10, marker());
	let profile = ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
	ensure!(profile < 3, Error::unsupported(Format::Vp9, format!("profile {profile}")));
	let show_existing_frame = (b >> 3) & 1 != 0;
	if show_existing_frame {
		return Ok(Vp9FrameHead {
			profile,
			show_existing_frame,
			keyframe: false,
			show_frame: true,
			error_resilient: false,
		});
	}
	let keyframe = (b >> 2) & 1 == 0;
	let show_frame = (b >> 1) & 1 != 0;
	let error_resilient = b & 1 != 0;
	if keyframe {
		ensure!(frame.len() >= 4, Error::truncated(Format::Vp9, 4, frame.len()));
		ensure!(
			frame[1..4] == [0x49, 0x83, 0x42],
			Error::malformed(Format::Vp9, "bad keyframe sync code")
		);
	}
	Ok(Vp9FrameHead { profile, show_existing_frame, keyframe, show_frame, error_resilient })
}
