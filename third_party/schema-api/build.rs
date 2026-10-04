use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn collect_protos(dir: &Path, recursive: bool, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && recursive {
            collect_protos(&path, true, files)?;
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            // Google well-known definitions come from the same distribution as protoc.
            // The archive's descriptor.proto/cpp_features.proto have invalid option syntax.
            if !path.starts_with("proto/google/protobuf") {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    println!("cargo:rerun-if-env-changed=PROTOC_INCLUDE");
    let out = PathBuf::from(env::var("OUT_DIR")?);
    let mut files = Vec::new();
    collect_protos(
        Path::new("proto"),
        env::var_os("CARGO_FEATURE_INFRASTRUCTURE").is_some(),
        &mut files,
    )?;
    files.sort();

    let mut config = prost_build::Config::new();
    config.protoc_executable(match env::var_os("PROTOC") {
        Some(path) => PathBuf::from(path),
        None => protoc_bin_vendored::protoc_bin_path()?,
    });
    config.enable_type_names();
    let standard_include = match env::var_os("PROTOC_INCLUDE") {
        Some(path) => PathBuf::from(path),
        None => protoc_bin_vendored::include_path()?,
    };
    let includes = [standard_include, PathBuf::from("proto")];
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(env::var_os("CARGO_FEATURE_SERVER").is_some())
        .include_file("packages.rs")
        .file_descriptor_set_path(out.join("api_descriptor.bin"))
        .compile_with_config(config, &files, &includes)?;
    Ok(())
}
