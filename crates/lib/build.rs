fn main() {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .exec()
        .expect("running cargo metadata");
    let workspace_manifest = metadata.workspace_root.join("Cargo.toml");
    println!("cargo::rerun-if-changed={workspace_manifest}");
    println!(
        "cargo::rerun-if-changed={}/Cargo.lock",
        metadata.workspace_root
    );

    let composefs = metadata
        .packages
        .iter()
        .find(|p| p.name == "composefs-oci")
        .expect("composefs-oci dependency");
    println!(
        "cargo::rustc-env=BOOTC_COMPOSEFS_BUILD={} {}",
        composefs.version,
        composefs
            .source
            .as_ref()
            .map(|s| s.repr.as_str())
            .unwrap_or("path")
    );

    let bins = metadata.workspace_metadata["binary-dependencies"]["bins"]
        .as_array()
        .expect("workspace.metadata.binary-dependencies.bins must be an array");
    let bins: Vec<&str> = bins
        .iter()
        .map(|bin| bin.as_str().expect("binary dependency must be a string"))
        .collect();
    println!("cargo::rustc-env=BOOTC_BINARY_DEPS={}", bins.join(","));
}
