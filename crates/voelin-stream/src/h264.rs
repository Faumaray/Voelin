//! H.264 `profile-level-id` for the offer: the profile our encoders produce
//! and the level the stream's size, frame rate and bitrate need (ITU-T
//! H.264 Table A-1), instead of a fixed level.

/// Profile of our H.264 streams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum H264Profile {
	/// Constrained High (`profile_idc` 100, `constraint_set4` and `5`): what
	/// TeamSpeak clients decode.
	#[default]
	ConstrainedHigh,
	/// Constrained Baseline (`profile_idc` 66, `constraint_set0` and `1`).
	ConstrainedBaseline,
}

/// Level limits: `level_idc`, macroblocks per second, macroblocks per frame,
/// bitrate in kbit/s (Baseline; High allows 1.25 times that).
const LEVELS: [(u8, u64, u64, u64); 19] = [
	(10, 1_485, 99, 64),
	(11, 3_000, 396, 192),
	(12, 6_000, 396, 384),
	(13, 11_880, 396, 768),
	(20, 11_880, 396, 2_000),
	(21, 19_800, 792, 4_000),
	(22, 20_250, 1_620, 4_000),
	(30, 40_500, 1_620, 10_000),
	(31, 108_000, 3_600, 14_000),
	(32, 216_000, 5_120, 20_000),
	(40, 245_760, 8_192, 20_000),
	(41, 245_760, 8_192, 50_000),
	(42, 522_240, 8_704, 50_000),
	(50, 589_824, 22_080, 135_000),
	(51, 983_040, 36_864, 240_000),
	(52, 2_073_600, 36_864, 240_000),
	(60, 4_177_920, 139_264, 240_000),
	(61, 8_355_840, 139_264, 480_000),
	(62, 16_711_680, 139_264, 800_000),
];

/// The lowest level (`level_idc`, e.g. 31 for 3.1) whose limits hold
/// `width` x `height` at `fps` and `bitrate` bit/s; the highest level
/// defined if none does (the stream is sent anyway: decoders mostly take
/// more than their level).
pub fn level_idc(profile: H264Profile, width: u32, height: u32, fps: u32, bitrate: u64) -> u8 {
	let (mbs_w, mbs_h) = (u64::from(width.div_ceil(16)), u64::from(height.div_ceil(16)));
	let frame = mbs_w * mbs_h;
	let rate = frame * u64::from(fps.max(1));
	let factor = match profile {
		H264Profile::ConstrainedHigh => 1250,
		H264Profile::ConstrainedBaseline => 1000,
	};
	LEVELS
		.iter()
		.find(|&&(_, max_rate, max_frame, max_kbps)| {
			rate <= max_rate
				&& frame <= max_frame
				// Neither side may exceed sqrt(8 * MaxFS) macroblocks.
				&& mbs_w * mbs_w <= 8 * max_frame
				&& mbs_h * mbs_h <= 8 * max_frame
				&& bitrate <= max_kbps * factor
		})
		.map_or(LEVELS[LEVELS.len() - 1].0, |l| l.0)
}

/// `profile-level-id` (`a=fmtp`, three bytes: `profile_idc`,
/// `profile-iop`, `level_idc`) for `profile` at `level_idc`.
pub fn profile_level_id(profile: H264Profile, level_idc: u8) -> u32 {
	let (profile_idc, iop) = match profile {
		H264Profile::ConstrainedHigh => (0x64, 0x0c),
		H264Profile::ConstrainedBaseline => (0x42, 0xe0),
	};
	(profile_idc << 16) | (iop << 8) | u32::from(level_idc)
}

/// The level TeamSpeak offers (3.1): our offers never go below it, so the
/// offer official clients know stays the same for small streams.
pub const MIN_OFFER_LEVEL: u8 = 31;

/// `profile-level-id` for a stream of this size, frame rate and bitrate,
/// at least level 3.1.
pub fn offer_profile_level_id(
	profile: H264Profile,
	width: u32,
	height: u32,
	fps: u32,
	bitrate: u64,
) -> u32 {
	let level = level_idc(profile, width, height, fps, bitrate).max(MIN_OFFER_LEVEL);
	profile_level_id(profile, level)
}

#[cfg(test)]
mod tests {
	use super::*;

	const HIGH: H264Profile = H264Profile::ConstrainedHigh;

	#[test]
	fn levels_from_macroblocks_per_second() {
		// 720p30: 3600 MBs per frame, 108000 per second: exactly 3.1.
		assert_eq!(level_idc(HIGH, 1280, 720, 30, 4_000_000), 31);
		// 720p60 needs 3.2, 1080p30 4.0, 1080p60 4.2.
		assert_eq!(level_idc(HIGH, 1280, 720, 60, 4_000_000), 32);
		assert_eq!(level_idc(HIGH, 1920, 1080, 30, 8_000_000), 40);
		assert_eq!(level_idc(HIGH, 1920, 1080, 60, 8_000_000), 42);
		// 1440p60: 14400 MBs per frame, more than 4.2's 8704: 5.1.
		assert_eq!(level_idc(HIGH, 2560, 1440, 60, 10_000_000), 51);
		// 4K120 needs 6.0 (and any size or rate beyond all: the top level).
		assert_eq!(level_idc(HIGH, 3840, 2160, 120, 10_000_000), 60);
		assert_eq!(level_idc(HIGH, 16384, 16384, 240, 10_000_000), 62);
		// Small streams: low levels.
		assert_eq!(level_idc(HIGH, 320, 240, 15, 200_000), 12);
		// The bitrate counts too: 1080p30 at 30 Mbit/s is beyond 4.0 (25
		// Mbit/s in High).
		assert_eq!(level_idc(HIGH, 1920, 1080, 30, 30_000_000), 41);
		// Baseline has the lower bitrate limit.
		let baseline = H264Profile::ConstrainedBaseline;
		assert_eq!(level_idc(baseline, 1920, 1080, 30, 22_000_000), 41);
		assert_eq!(level_idc(HIGH, 1920, 1080, 30, 22_000_000), 40);
		// A very wide frame is limited by sqrt(8 * MaxFS).
		assert_eq!(level_idc(HIGH, 4096, 64, 1, 100_000), 40);
	}

	#[test]
	fn profile_level_ids() {
		assert_eq!(profile_level_id(HIGH, 31), 0x640c1f);
		assert_eq!(profile_level_id(H264Profile::ConstrainedBaseline, 31), 0x42e01f);
		assert_eq!(offer_profile_level_id(HIGH, 320, 240, 15, 200_000), 0x640c1f);
		assert_eq!(offer_profile_level_id(HIGH, 1920, 1080, 60, 8_000_000), 0x640c2a);
	}
}
