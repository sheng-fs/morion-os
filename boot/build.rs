/// Morion Boot 构建脚本
///
/// 职责：
/// 1. 编译汇编引导桩 (boot_stub.asm) 并链接
/// 2. 嵌入预计算的亚克力模糊核查找表
/// 3. 生成版本信息常量
/// 4. 将 Nix store 路径下的引导配置嵌入二进制

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asm_dir = Path::new("asm");

    // ============================================================
    // 1. 编译汇编引导桩
    // ============================================================
    let asm_path = asm_dir.join("boot_stub.asm");
    if asm_path.exists() {
        let asm_output = out_dir.join("boot_stub.o");
        let _target = "x86_64-unknown-uefi";

        // 使用 NASM 编译为 x86_64 UEFI COFF 目标
        let status = std::process::Command::new("nasm")
            .args([
                "-f", "win64",           // x86_64 Windows/EFI COFF 格式
                "-o", asm_output.to_str().unwrap(),
                asm_path.to_str().unwrap(),
            ])
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("cargo:rustc-link-arg={}", asm_output.display());
                println!("cargo:rerun-if-changed={}", asm_path.display());
            }
            Ok(s) => {
                eprintln!("cargo:warning=NASM 编译失败，使用纯 Rust 入口");
                eprintln!("cargo:warning=退出码: {:?}", s.code());
            }
            Err(e) => {
                eprintln!("cargo:warning=未找到 NASM，使用纯 Rust 入口: {}", e);
            }
        }
    }

    // ============================================================
    // 2. 预计算亚克力模糊核 (高斯近似核，整数运算)
    // ============================================================
    let blur_radius: usize = 3;
    let kernel_size = 2 * blur_radius + 1;
    let sigma: f64 = 1.5;
    let mut kernel = vec![0u16; kernel_size * kernel_size];
    let mut kernel_sum: f64 = 0.0;

    for y in 0..kernel_size {
        for x in 0..kernel_size {
            let dx = x as f64 - blur_radius as f64;
            let dy = y as f64 - blur_radius as f64;
            let val = (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
            kernel[y * kernel_size + x] = (val * 4096.0) as u16; // Q12.4 定点
            kernel_sum += val;
        }
    }

    // 归一化确保总和不溢出
    let scale = (4096.0 / kernel_sum) as u64;
    for v in kernel.iter_mut() {
        *v = ((*v as u64 * scale) >> 8).min(65535) as u16;
    }

    // 生成 Rust 代码嵌入模糊核
    let blur_code = format!(
        "pub const BLUR_RADIUS: usize = {};\n\
         pub const KERNEL_SIZE: usize = {};\n\
         pub const BLUR_KERNEL: [u16; {}] = {:?};\n\
         pub const KERNEL_SCALE: u32 = 4096;\n",
        blur_radius,
        kernel_size,
        kernel_size * kernel_size,
        kernel,
    );
    fs::write(out_dir.join("blur_kernel.rs"), blur_code).unwrap();

    // ============================================================
    // 3. 生成版本信息
    // ============================================================
    let version_code = format!(
        r#"pub const BOOT_VERSION: &str = "{}";
pub const BOOT_COMMIT: &str = "{}";
pub const BOOT_BUILD_DATE: &str = "{}";
"#,
        env!("CARGO_PKG_VERSION"),
        option_env!("GIT_HASH").unwrap_or("unknown"),
        chrono_now(),
    );
    fs::write(out_dir.join("version.rs"), version_code).unwrap();

    // ============================================================
    // 4. 嵌入默认引导配置路径
    // ============================================================
    println!("cargo:rustc-env=BOOT_CONFIG_DIR=/boot/loader");
    println!("cargo:rustc-env=NIX_STORE_DIR=/nix/store");
    println!("cargo:rerun-if-changed=loader/");
    println!("cargo:rerun-if-changed=build.rs");
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // UTC 日期格式化 (简化版)
    let days_since_epoch = secs / 86400;
    let mut year = 1970;
    let mut remaining = days_since_epoch as i64;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if remaining < year_days {
            break;
        }
        remaining -= year_days;
        year += 1;
    }
    let months = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1;
    for &m in months.iter() {
        if remaining < m as i64 {
            break;
        }
        remaining -= m as i64;
        month += 1;
    }
    let day = remaining + 1;
    format!("{:04}-{:02}-{:02}", year, month, day)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
