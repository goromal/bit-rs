fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=AAPIS_PROTO_DIR");
    let root = std::env::var("AAPIS_PROTO_DIR").unwrap_or_else(|_| "../aapis/protos".into());
    let proto = format!("{root}/aapis/bit/v1/bit.proto");
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure().compile_protos(&[proto], &[root])?;
    Ok(())
}
