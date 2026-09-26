# Called from Rust through JNI by name.
-keep class io.github.faumaray.tsc.Bridge { *; }
-keep class io.github.faumaray.tsc.Native { *; }
-keep class io.github.faumaray.tsc.TscApp { *; }

# rustls-platform-verifier's Kotlin component, also called from Rust.
-keep class org.rustls.platformverifier.** { *; }
