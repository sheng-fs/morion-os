//! LibDevice — 用户态设备驱动的公共底座（驱动路线 D2）。
//!
//! 只放**与"我是服务进程还是飞地应用"无关**的东西：
//!
//! - [`grant`]：内核 → 驱动的**通用设备授权描述**（`DeviceGrant`），单一来源，各驱动不再各抄一份。
//! - [`mmio`]：易失 MMIO / DMA 读写原语 + 写序栅栏。
//! - [`msix`]：MSI-X 表项写入（表由驱动写，配置空间写留在内核）。
//! - [`virtio`]（D2b）：virtio **modern 传输层 + vring 原语** —— 能力解析 / common cfg /
//!   复位协商 / 队列配置 / avail·used 环，`net_srv` 与 `virtio_blk_srv` 共用。只放「所有
//!   virtio 设备都一样」的那层，**设备语义**（包头、请求链）仍在各自驱动里。
//!
//! **两种形态**都链接它：服务形态（如 `block_srv` 对外暴露 IPC 协议）与直通形态（飞地应用
//! 运行时直接 MMIO/DMA）。驱动核心因此**不感知自己被谁包裹** —— 这是 E3「同一 LibDevice，
//! 两种形态行为一致」的前提。
//!
//! 本库保持**零依赖**（`virtio::discover_caps` 把「读 PCI 配置空间」做成参数而非依赖
//! `morion`），故飞地直通形态也能原样复用。

#![no_std]

pub mod grant;
pub mod mmio;
pub mod msix;
pub mod virtio;
