//! Build script: embed the compilation target triple into the binary so
//! `vmfleet self-update` can pick the matching release asset
//! (`vmfleet-<version>-<target>.tar.gz`, produced by `.github/workflows/release.yml`).
//! `TARGET` is provided to build scripts by cargo.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=VMFLEET_TARGET={target}");
}
