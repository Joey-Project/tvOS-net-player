fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let protobuf_include = protoc_bin_vendored::include_path()?;
    // SAFETY: build scripts run single-threaded before code generation starts.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    let proto_root = std::path::PathBuf::from("../../Sources/TVOSNetPlayerCacheClient/Protos");
    let proto_path = proto_root.join("tvos_net_player/v1/cache_control.proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &[proto_path.as_path()],
            &[proto_root.as_path(), protobuf_include.as_path()],
        )?;
    println!("cargo:rerun-if-changed={}", proto_path.display());
    Ok(())
}
