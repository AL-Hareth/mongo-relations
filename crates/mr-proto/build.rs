fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    // Use a vendored protoc unless one is provided explicitly.
    let protoc = match std::env::var_os("PROTOC") {
        Some(p) => p.into(),
        None => protoc_bin_vendored::protoc_bin_path()?,
    };
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_with_config(
            config,
            &["proto/mongo_relations/v1/relations.proto"],
            &["proto"],
        )?;
    Ok(())
}
