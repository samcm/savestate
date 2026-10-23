fn main() -> Result<(), Box<dyn std::error::Error>> {
    let guest = "../../proto/savestate/guest/v1/guest.proto";
    prost_build::Config::new()
        // A vendored protoc, so builds need no system protobuf compiler.
        .protoc_executable(protoc_bin_vendored::protoc_bin_path()?)
        .compile_protos(&[guest], &["../../proto"])?;
    println!("cargo:rerun-if-changed={guest}");
    Ok(())
}
