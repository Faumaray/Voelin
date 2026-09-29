//! The Stream Studio: scenes of sources composited into one picture that the
//! streamer encodes, plus outputs that keep or forward the encoded packets.
//!
//! - [`scene`]: the scene graph (scenes, sources, transforms, crops), the
//!   persisted shape of a studio
//! - [`compose`]: the compositor, which draws the live scene into pooled
//!   frames at the output size

pub mod compose;
pub mod scene;
