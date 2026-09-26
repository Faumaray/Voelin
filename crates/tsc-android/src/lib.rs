//! The Android app's native library (`libtsc_android.so`).
//!
//! The APK (`android/`) starts a `NativeActivity` subclass that loads this
//! library; `android_main` then runs the shared Slint UI (`tsc_ui`) on
//! Slint's Android backend. The pieces around it:
//!
//! - [`host`]: one engine per process, so voice survives the activity being
//!   destroyed and recreated; a new window gets the current state replayed
//! - [`foreground`]: when the voice foreground service must run (Android
//!   only lets a service with a notification keep the microphone in the
//!   background)
//! - `bridge` (Android): JNI calls into the Kotlin side (`Bridge`) and the
//!   native methods it calls back (`Native`)
//! - `capture` (Android): screen and system-audio capture through
//!   MediaProjection, registered as `tsc_media` capture providers
//! - `secrets` (Android): passwords encrypted with an Android Keystore key
//!
//! Only [`host`] and [`foreground`] build on other platforms, for tests.

pub mod foreground;
pub mod host;

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
mod capture;
#[cfg(target_os = "android")]
mod secrets;
