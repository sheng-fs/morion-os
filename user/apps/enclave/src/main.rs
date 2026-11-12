//! 性能飞地最小示例（docs/architecture.md §6.2）。
//!
//! 流程：**申请窗口 → 读寄存器 → 打印 → 退出**，并把失败路径完整打出来：
//!
//! 1. 用 `libdevice` 取本域的设备授权描述（`DeviceGrant`）；没有设备时打印失败并退出
//!    —— 这正是"未接入构建"的示例在当前镜像里的预期路径。
//! 2. 调 [`sys_enclave_map`] 把已授权的 DMA 块绑到独立 IOVA 窗口，并让内核把设备 BAR
//!    映射进本域。
//! 3. 成功则用 `libdevice::mmio` 以 volatile 读 BAR0 的第一个 dword（非缓存映射，必须
//!    volatile）。
//!
//! 失败不会破坏既有驱动：内核在任何一步不支持时都保持原来的恒等映射（见内核
//! `device::syscall_enclave_map`）。本示例不参与本批镜像构建。

#![no_std]
#![no_main]

use libdevice::grant::DeviceGrant;
use libdevice::mmio;
use morion::syscall::{
    print, print_hex, print_u64, println, sys_enclave_map, EnclaveWindow, ENCLAVE_DEVICE_SELF,
    ENCLAVE_ERR_BAD_WINDOW, ENCLAVE_ERR_CONFLICT, ENCLAVE_ERR_NO_CAP, ENCLAVE_ERR_NO_DEVICE,
    ENCLAVE_ERR_UNSUPPORTED,
};

/// 期望的 IOVA 基址：256 MiB，页对齐，且避开 0（内核用 0 表示"未绑定"）。
const WANT_IOVA: u64 = 0x1000_0000;
/// 演示用寄存器偏移（BAR0 的第一个 dword）。
const REG_OFFSET: u64 = 0;

#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    println("enclave: 性能飞地窗口示例启动");

    // 1. 取本域设备授权。没有设备时 `magic` 不匹配（内核映射的是零页）。
    let grant = DeviceGrant::load();
    if !grant.is_valid() {
        println("enclave: 本域无设备授权 -> 飞地不可用（失败路径，预期）");
        return;
    }
    print("enclave: BAR=0x");
    print_hex(grant.bar_paddr);
    print(" bar_vaddr=0x");
    print_hex(grant.bar_vaddr);
    print(" DMA=0x");
    print_hex(grant.dma_paddr);
    print(" bytes=0x");
    print_hex(grant.dma_bytes);
    println("");

    // 2. 申请窗口：BAR 映射进本域 + 已授权 DMA 块绑到独立 IOVA 窗口。
    let win: EnclaveWindow = match sys_enclave_map(ENCLAVE_DEVICE_SELF, WANT_IOVA, grant.dma_bytes)
    {
        Ok(w) => w,
        Err(e) => {
            print("enclave: 申请窗口失败 err=");
            print_u64(e);
            match e {
                ENCLAVE_ERR_NO_DEVICE => println(" (无设备 / 选择不匹配)"),
                ENCLAVE_ERR_NO_CAP => println(" (缺少 Mmio 能力)"),
                ENCLAVE_ERR_BAD_WINDOW => println(" (窗口非法或越界)"),
                ENCLAVE_ERR_UNSUPPORTED => println(" (IOMMU 不可用，保持恒等映射)"),
                ENCLAVE_ERR_CONFLICT => println(" (BAR 窗口冲突)"),
                _ => println(" (未知错误)"),
            }
            return;
        }
    };

    print("enclave: 窗口已建立 iova=0x");
    print_hex(win.iova());
    print(" bytes=0x");
    print_hex(win.bytes());
    println("");

    // 3. 读寄存器：BAR 是非缓存映射，必须 volatile 读。
    let value = mmio::rd32(grant.bar_vaddr + REG_OFFSET);
    print("enclave: BAR0[0]=0x");
    print_hex(value as u64);
    println("");
    println("enclave: 完成，退出");
}
