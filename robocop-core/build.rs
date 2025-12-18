fn main() {
    built::write_built_file().expect("Failed to acquire build-time information");

    // Pass through ROBOCOP_GIT_HASH from Nix build environment
    println!("cargo:rerun-if-env-changed=ROBOCOP_GIT_HASH");
    if let Ok(hash) = std::env::var("ROBOCOP_GIT_HASH") {
        println!("cargo:rustc-env=ROBOCOP_GIT_HASH={}", hash);
    }
}
