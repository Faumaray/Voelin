# Called from Rust through JNI by name.
-keep class io.github.faumaray.voelin.Bridge { *; }
-keep class io.github.faumaray.voelin.Native { *; }
-keep class io.github.faumaray.voelin.VoelinApp { *; }

# rustls-platform-verifier's Kotlin component, also called from Rust.
-keep class org.rustls.platformverifier.** { *; }
