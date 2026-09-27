//! 构建脚本 — 复用 `user/linker.ld`（所有用户程序共用同一套链接地址布局）。
//!
//! 与 `user/build.rs` 的差别只在脚本路径: 这个程序是**独立 crate**, 产物是一份
//! 独立的 ELF（由内核 `SYS_SPAWN_ELF` 运行时载入）, 而不是塞进主程序那 17000 行里。

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let linker = std::path::Path::new(&manifest).join("../linker.ld");
    println!("cargo:rerun-if-changed={}", linker.display());
    println!("cargo:rustc-link-arg=-T{}", linker.display());
    // 自定义 target (code-model=large) 不继承工作区 config 的 -nostdlib。
    println!("cargo:rustc-link-arg=-nostdlib");
}
