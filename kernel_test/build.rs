//! 构建脚本 — 通过绝对路径指定链接脚本

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", std::env::var("CARGO_MANIFEST_DIR").unwrap());
}