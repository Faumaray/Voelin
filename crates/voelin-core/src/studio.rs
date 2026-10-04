//! The Stream Studio (feature `media`): [`voelin_media::studio`] with its
//! scenes and replay buffer kept in the settings.
//!
//! [`start`] runs a [`Studio`] from `studio.scenes` and keeps the two in
//! step: every edit made through [`Studio::apply`] is written back to
//! `studio.scenes`, and a change of `studio.scenes`, `studio.replay_seconds`
//! or `studio.replay_memory_mb` made anywhere else (a settings page,
//! `--set`) is applied to the running studio.
//!
//! The UI drives the studio with [`Command`]s, follows its [`Event`]s, and
//! draws [`Studio::preview`]. Its composite becomes the video of a stream
//! through [`crate::media::Streamer::start_studio`], which also feeds the
//! studio's outputs (recording, replay buffer, WHIP) with the packets the
//! stream's encoders make, whether or not the stream is live. Going live is
//! the stream's as usual (`Command::StartStream`, then attaching its sink);
//! [`Command::GoLive`] tells the studio, for its header and its WHIP
//! outputs.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use tokio::sync::broadcast::error::RecvError;
use tracing::warn;
pub use voelin_media::studio::*;

use crate::media::MediaError;
use crate::settings::{
	STUDIO_RECORDING_DIR, STUDIO_REPLAY_MEMORY_MB, STUDIO_REPLAY_SECONDS, STUDIO_SCENES, Settings,
};

/// Start a studio from `settings` and keep it and them in step (see the
/// [module docs](self)) for as long as it runs. Must run on a Tokio runtime.
pub async fn start(settings: &Settings) -> Result<Arc<Studio>, MediaError> {
	let scenes = (*settings.get_arc(&STUDIO_SCENES)).clone();
	let studio = Arc::new(Studio::start(scenes).await?);
	studio.apply(replay(settings)).await?;
	tokio::spawn(follow(Arc::downgrade(&studio), studio.events(), settings.clone()));
	Ok(studio)
}

/// Where recordings and clips go: `studio.recording_dir`, else a `Voelin`
/// folder in the user's videos folder.
pub fn recording_dir(settings: &Settings) -> PathBuf {
	let dir = settings.get(&STUDIO_RECORDING_DIR);
	if !dir.trim().is_empty() {
		return PathBuf::from(dir);
	}
	dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir).join("Voelin")
}

fn replay(settings: &Settings) -> Command {
	Command::SetReplay {
		seconds: settings.get(&STUDIO_REPLAY_SECONDS),
		memory_mb: settings.get(&STUDIO_REPLAY_MEMORY_MB),
	}
}

/// Write the studio's scene edits to the settings and apply the settings'
/// changes to the studio, until the studio is gone. Equal values are not
/// written back, so neither side echoes the other.
async fn follow(
	studio: Weak<Studio>,
	mut events: tokio::sync::broadcast::Receiver<Event>,
	settings: Settings,
) {
	let mut scenes = settings.watch(&STUDIO_SCENES);
	let mut seconds = settings.watch(&STUDIO_REPLAY_SECONDS);
	let mut memory = settings.watch(&STUDIO_REPLAY_MEMORY_MB);
	loop {
		let command = tokio::select! {
			event = events.recv() => {
				let edited = match event {
					Ok(Event::Scenes(edited)) => edited,
					Ok(_) => continue,
					// Missed some: the studio's scenes now are what counts.
					Err(RecvError::Lagged(_)) => match studio.upgrade() {
						Some(studio) => Arc::new(studio.scenes()),
						None => return,
					},
					Err(RecvError::Closed) => return,
				};
				if *settings.get_arc(&STUDIO_SCENES) != *edited
					&& let Err(e) = settings.set(&STUDIO_SCENES, (*edited).clone())
				{
					warn!("cannot keep the studio's scenes: {e}");
				}
				continue;
			}
			Some(changed) = scenes.changed() => Command::SetScenes(Box::new((*changed).clone())),
			Some(_) = seconds.changed() => replay(&settings),
			Some(_) = memory.changed() => replay(&settings),
			else => return,
		};
		let Some(studio) = studio.upgrade() else { return };
		if let Command::SetScenes(changed) = &command
			&& studio.scenes() == **changed
		{
			continue;
		}
		if let Err(e) = studio.apply(command).await {
			warn!("studio settings: {e}");
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use super::scene::{Colour, Scene, Scenes, SourceKind};
	use super::*;

	async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
		let started = Instant::now();
		while !check() {
			assert!(started.elapsed() < Duration::from_secs(5), "{what}");
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn the_studio_and_its_settings_follow_each_other() {
		let settings = Settings::in_memory();
		let mut stored = Scenes { width: 64, height: 48, ..Scenes::default() };
		stored.scenes.push(Scene::new(1, "Main"));
		settings.set(&STUDIO_SCENES, stored.clone()).unwrap();
		let studio = start(&settings).await.unwrap();
		assert_eq!(studio.scenes(), stored);

		// An edit in the studio is kept in the settings.
		studio
			.apply(Command::AddSource {
				scene: 1,
				name: "Slate".into(),
				kind: SourceKind::Colour { colour: Colour::rgb(1, 2, 3), size: (64, 48) },
			})
			.await
			.unwrap();
		eventually("the edit reached the settings", || {
			settings.get(&STUDIO_SCENES).scenes[0].sources.len() == 1
		})
		.await;

		// A change of the setting reaches the studio.
		let mut changed = settings.get(&STUDIO_SCENES);
		changed.scenes.push(Scene::new(2, "Break"));
		changed.active = 2;
		settings.set(&STUDIO_SCENES, changed.clone()).unwrap();
		eventually("the setting reached the studio", || studio.scenes() == changed).await;
		eventually("the studio switched", || studio.status().active_scene == 2).await;

		// And so does the replay window: on with a keyframe wanted, off without.
		settings.set(&STUDIO_REPLAY_SECONDS, 0).unwrap();
		eventually("the replay buffer is off", || !studio.needs_keyframe(0)).await;
		settings.set(&STUDIO_REPLAY_SECONDS, 5).unwrap();
		eventually("the replay buffer is on", || studio.needs_keyframe(0)).await;

		// Invalid scenes never get into the settings.
		let mut twice = changed.clone();
		twice.scenes.push(Scene::new(1, "Again"));
		assert!(settings.set(&STUDIO_SCENES, twice).is_err());
	}

	#[test]
	fn recordings_go_to_the_setting_or_the_videos_folder() {
		let settings = Settings::in_memory();
		assert!(recording_dir(&settings).ends_with("Voelin"));
		settings.set(&STUDIO_RECORDING_DIR, "/srv/clips".into()).unwrap();
		assert_eq!(recording_dir(&settings), PathBuf::from("/srv/clips"));
	}
}
