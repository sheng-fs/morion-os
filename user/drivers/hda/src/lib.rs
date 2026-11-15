//! MorionOS 通用 HDA 音频驱动（Intel High Definition Audio；AMD 平台同规范）。
//!
//! 目标是"一个通用驱动 + 一个可替换的 codec 配置槽"：
//!
//! - [`io::HdaIo`]：寄存器访问抽象。真机用 [`io::MmioHdaIo`]（libdevice MMIO 原语），
//!   单测用内存里的假寄存器；核心逻辑因此可以在宿主机上完整跑通。
//! - [`regs`]：HDA 寄存器偏移、verb 编码、Stream Format 位域。
//! - [`chips`]：已知控制器 / codec 芯片表（Realtek ALC、Conexant、IDT、VIA、ADI、
//!   Intel/AMD/NVIDIA HDMI 等），提供可读名字与厂商级 quirks。
//! - [`resources`]：把 `DeviceGrant` 的 DMA 块切成 CORB / RIRB / BDL / PCM，并做对齐检查。
//! - [`codec`]：通用 codec 发现（root → AFG → DAC/Pin），以及 [`CodecProfile`] 覆盖槽。
//! - [`stream`]：BDL 与循环 PCM 缓冲、LPIB 轮询（不依赖中断）。
//! - [`driver`]：实现 `libdevmodel::Driver` / `AudioOps` 的服务侧入口。
//!
//! 只在测试时启用 `std`；驱动本体 `no_std`、零分配、零第三方依赖。
//!
//! # 可替换点
//!
//! 1. **寄存器层**：换掉 [`io::HdaIo`] 即可把同一套核心用于不同的访问方式
//!    （例如内核内直通、受控的 I/O 端口代理）。
//! 2. **codec 配置**：实现 [`codec::CodecProfile`] 返回 `CodecQuirks`，覆盖 DAC/Pin、
//!    pin control、EAPD、额外 verb 序列；通用发现代码一行不改。
//!
//! # 明确未做
//!
//! 中断 / MSI-X 完成通知、录音（ADC）路径、多 codec 并发、多流同时播放；播放位置目前
//! 靠轮询 SDnLPIB。厂商 codec 的深度 quirks（针脚重映射等）仍需按具体机型补。

#![cfg_attr(not(test), no_std)]

pub mod chips;
pub mod codec;
pub mod driver;
pub mod io;
pub mod regs;
pub mod resources;
mod stream;

pub use codec::{CodecInfo, CodecProfile, CodecQuirks, GenericProfile};
pub use driver::{HdaDriver, MmioHdaDriver, SUPPORTED_FORMATS};
pub use io::{HdaIo, HdaLog, MmioHdaIo, SilentLog};
pub use resources::{DmaLayout, HdaResources};

#[cfg(test)]
mod tests;
