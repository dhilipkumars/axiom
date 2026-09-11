//! Generates the tonic/prost client stubs for `proto/axiom/v1/axiom.proto`.
//! Uses a vendored `protoc` so the build does not depend on a system protoc.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("../proto");
    let proto = proto_root.join("axiom/v1/axiom.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    let include = protoc_bin_vendored::include_path()?;

    println!("cargo:rerun-if-changed={}", proto.display());
    tonic_build::configure()
        // The server side is only used by the in-process stub gateway in tests,
        // but generating it unconditionally keeps the build simple.
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[proto_root, include])?;
    Ok(())
}
