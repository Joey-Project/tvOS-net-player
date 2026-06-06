fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: build scripts run single-threaded before code generation starts.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    let proto_path =
        "../../Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto";
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &[proto_path],
            &["../../Sources/TVOSNetPlayerCacheClient/Protos"],
        )?;
    println!("cargo:rerun-if-changed={proto_path}");
    Ok(())
}
