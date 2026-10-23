fn main() -> Result<(), Box<dyn std::error::Error>> {
    let launcher = "../../proto/savestate/launcher/v1/launcher.proto";
    let mut config = prost_build::Config::new();
    // A vendored protoc, so builds need no system protobuf compiler.
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure().compile_with_config(config, &[launcher], &["../../proto"])?;
    println!("cargo:rerun-if-changed={launcher}");
    Ok(())
}
