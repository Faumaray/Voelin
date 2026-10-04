import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

/** The Cargo workspace the native library comes from. */
val workspace: File = rootDir.parentFile

/** `version` of crates/voelin-android, which is the app's version (docs/release.md). */
val crateVersion: String = workspace.resolve("crates/voelin-android/Cargo.toml").readLines()
    .first { it.startsWith("version = ") }
    .substringAfter('"').substringBefore('"')

/** The version of `crate` in Cargo.lock. */
fun lockedVersion(crate: String): String {
    val lines = workspace.resolve("Cargo.lock").readLines()
    val at = lines.indexOfFirst { it == "name = \"$crate\"" }
    require(at >= 0) { "$crate is not in Cargo.lock" }
    return lines[at + 1].substringAfter('"').substringBefore('"')
}

/**
 * A release signing setting: a Gradle property (e.g. in
 * ~/.gradle/gradle.properties) or an environment variable (CI secrets).
 * Never stored in the repository.
 */
fun signing(property: String, env: String): String? =
    providers.gradleProperty(property).orElse(providers.environmentVariable(env)).orNull

val releaseStoreFile = signing("voelin.signing.storeFile", "VOELIN_SIGNING_STORE_FILE")

android {
    namespace = "io.github.faumaray.voelin"
    compileSdk = 36
    // What the release workflow and the Docker build install (AGP's
    // default would be 35.0.0).
    buildToolsVersion = "36.0.0"
    ndkVersion = "27.3.13750724"

    defaultConfig {
        // The desktop app id (packaging/README.md); it cannot change after
        // the first upload to Google Play.
        applicationId = "io.github.faumaray.Voelin"
        minSdk = 29
        targetSdk = 36
        versionName = crateVersion
        // major * 1000000 + minor * 1000 + patch (docs/release.md).
        versionCode = crateVersion.split('.').map(String::toInt)
            .let { (major, minor, patch) -> major * 1_000_000 + minor * 1_000 + patch }
    }

    signingConfigs {
        if (releaseStoreFile != null) {
            create("release") {
                storeFile = file(releaseStoreFile)
                storePassword = signing("voelin.signing.storePassword", "VOELIN_SIGNING_STORE_PASSWORD")
                keyAlias = signing("voelin.signing.keyAlias", "VOELIN_SIGNING_KEY_ALIAS")
                keyPassword = signing("voelin.signing.keyPassword", "VOELIN_SIGNING_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // Unsigned without the signing settings.
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // Stored uncompressed and page-aligned, loaded straight from the APK.
        jniLibs.useLegacyPackaging = false
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    // Must be the version of the rustls-platform-verifier-android crate.
    implementation("org.rustls:rustls-platform-verifier:${lockedVersion("rustls-platform-verifier-android")}")
}

// libvoelin_android.so, built with cargo-ndk into build/rust/<build type>/<abi>/
// before the JNI libraries are merged. -Pvoelin.skipCargo=true uses what is
// there (e.g. built by CI in an earlier step).
val abis = providers.gradleProperty("voelin.abis").getOrElse("arm64-v8a,x86_64")
    .split(',').map(String::trim).filter(String::isNotEmpty)
val skipCargo = providers.gradleProperty("voelin.skipCargo").map(String::toBoolean).getOrElse(false)

for ((buildType, release) in listOf("debug" to false, "release" to true)) {
    val out = layout.buildDirectory.dir("rust/$buildType")
    val name = buildType.replaceFirstChar(Char::uppercase)
    val cargo = tasks.register<Exec>("cargoBuild$name") {
        group = "build"
        description = "Builds the Rust library for $abis with cargo-ndk ($buildType)."
        workingDir = workspace
        val command = mutableListOf("cargo", "ndk", "--platform", "29", "-o", out.get().asFile.path)
        abis.forEach { command += listOf("-t", it) }
        command += listOf("build", "--locked", "-p", "voelin-android")
        if (release) command += "--release"
        commandLine(command)
        val ndk = android.ndkDirectory.path
        environment("ANDROID_NDK_HOME", ndk)
        // CMake (the bundled libopus) finds the NDK through this one.
        environment("ANDROID_NDK_ROOT", ndk)
        environment("ANDROID_HOME", android.sdkDirectory.path)
        // Slint compiles its Java helper against this.
        environment(
            "ANDROID_JAR",
            android.sdkDirectory.resolve("platforms/android-${android.compileSdk}/android.jar").path,
        )
    }
    android.sourceSets.getByName(buildType).jniLibs.srcDir(out)
    if (!skipCargo) {
        tasks.configureEach {
            if (this.name == "merge${name}JniLibFolders") dependsOn(cargo)
        }
    }
}
