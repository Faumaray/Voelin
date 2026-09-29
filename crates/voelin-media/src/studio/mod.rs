//! The Stream Studio: scenes of sources composited into one picture that the
//! streamer encodes, plus outputs that keep or forward the encoded packets.
//!
//! - [`scene`]: the scene graph (scenes, sources, transforms, crops), the
//!   persisted shape of a studio
//! - [`compose`]: the compositor, which draws the live scene into pooled
//!   frames at the output size
//! - [`source`]: the live input behind each of a scene's sources (colour,
//!   text, image, screen, window, camera)
//! - [`camera`]: listing and capturing cameras
//! - [`output`]: what the encoded packets go to besides the stream
//!   (recording, the replay buffer, WHIP)

pub mod camera;
pub mod compose;
pub mod output;
pub mod scene;
pub mod source;
