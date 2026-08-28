//! Morion Boot — 声明式微引导加载器
//!
//! 架构概述:
//!
//! ```text
//! UEFI 固件
//!   │
//!   ├── GOP (Graphics Output Protocol) → 高分辨率显示
//!   ├── SimpleFileSystem → ESP 文件读取
//!   ├── Secure Boot → 签名验证
//!   └── TPM 2.0 → 可信测量
//!   │
//!   ▼
//!  [汇编引导桩 boot_stub.asm]
//!   │  最小可信链 — 仅做必要的寄存器初始化和安全自检
//!   │  保留完整的 UEFI Handoff 信息
//!   │
//!   ▼
//!  [Rust UEFI 入口 efi_main]
//!   │
//!   ├─► 1. 初始化 GOP 帧缓冲 (高分辨率真彩色)
//!   ├─► 2. 加载 theme.toml 和 loader.conf (声明式配置)
//!   ├─► 3. 扫描 Nix store 引导条目 (多代并存)
//!   ├─► 4. Secure Boot 验证内核签名
//!   ├─► 5. TPM 2.0 PCR 测量 (构建可信链)
//!   ├─► 6. 渲染亚克力引导菜单 (GOP + 整数模糊)
//!   ├─► 7. 处理用户输入 (键盘, 可选鼠标)
//!   ├─► 8. 加载选定的内核 + initrd
//!   └─► 9. ExitBootServices → 跳转到内核
//! ```

#![no_std]
#![no_main]
#![allow(invalid_reference_casting)]

extern crate alloc;

// ============================================================
// 模块声明
// ============================================================
pub mod gfx;
pub mod config;
pub mod security;
pub mod boot;