fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("generated.rs"), "pub const GENERATED: usize = 3;\n").unwrap();
    println!("cargo::rustc-cfg=fixture_active");
    println!("cargo::rustc-check-cfg=cfg(fixture_active, fixture_allowed_only)");
    println!("cargo::rerun-if-changed=build.rs");
}
