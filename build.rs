fn main() {
    // workaround for https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html
    println!("cargo::rustc-check-cfg=cfg(loom)");
}
