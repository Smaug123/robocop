fn main() {
    // Pass through ROBOCOP_GIT_HASH from the Nix build environment.
    println!("cargo:rerun-if-env-changed=ROBOCOP_GIT_HASH");
    if let Ok(hash) = std::env::var("ROBOCOP_GIT_HASH") {
        println!("cargo:rustc-env=ROBOCOP_GIT_HASH={}", hash);
    }
}
