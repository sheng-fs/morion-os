//! 性能飞地最小示例（docs/architecture.md §6.2）。
//!
//! 流程：**自动申请窗口 → 读寄存器 → 打印 → 解绑 → 退出**，并把失败路径完整打出来：
//!
//! 1. 用 `libdevice` 取本域的设备授权描述（`DeviceGrant`）；没有设备时打印失败并退出
//!    —— 这正是"未接入构建"的示例在当前镜像里的预期路径。
//! 2. 调 [`SYS_ENCLAVE_MAP`] 把已授权的 DMA 块绑到独立 IOVA 窗口（`a2 == 0` = 由内核
//!    自动分配），并让内核把设备 BAR 映射进本域。
//! 3. 成功则用 `libdevice::mmio` 以 volatile 读 BAR0 的第一个 dword（非缓存映射，必须
//!    volatile）。
//! 4. 调 [`SYS_ENCLAVE_DETACH`] 解除设备独占、恢复恒等映射并归还 IOVA。
//!
//! 失败不会破坏既有驱动：内核在任何一步不支持时都保持原来的恒等映射（见内核
//! `device::syscall_enclave_map`）。本示例不参与本批镜像构建。
//!
//! ⚠️ 这里直接发原始系统调用（而不是用 `morion::syscall::sys_enclave_map`）：本批只允许改
//! 本文件，而 `libmorion` 的错误码集合还没有新增的两个哨兵（`ALREADY_OWNED` / `NO_IOVA`）。

#![no_std]
#![no_main]

use core::arch::asm;

use libdevice::grant::DeviceGrant;
use libdevice::mmio;
use morion::syscall::{print, print_hex, print_u64, println};

/// 与内核 `syscall.rs` 的 `SYS_ENCLAVE_MAP` / `SYS_ENCLAVE_DETACH` 保持一致。
const SYS_ENCLAVE_MAP: u64 = 68;
const SYS_ENCLAVE_DETACH: u64 = 69;

/// `SYS_ENCLAVE_MAP` 的设备选择哨兵：本域已绑定的那台设备。
const DEVICE_SELF: u64 = u64::MAX;

/// 内核 `device::ENCLAVE_ERR_*`（`u64::MAX - n`）。
const ENCLAVE_ERR_NO_DEVICE: u64 = u64::MAX;
const ENCLAVE_ERR_NO_CAP: u64 = u64::MAX - 1;
const ENCLAVE_ERR_BAD_WINDOW: u64 = u64::MAX - 2;
const ENCLAVE_ERR_UNSUPPORTED: u64 = u64::MAX - 3;
const ENCLAVE_ERR_CONFLICT: u64 = u64::MAX - 4;
const ENCLAVE_ERR_ALREADY_OWNED: u64 = u64::MAX - 5;
const ENCLAVE_ERR_NO_IOVA: u64 = u64::MAX - 6;

/// 演示用寄存器偏移（BAR0 的第一个 dword）。
const REG_OFFSET: u64 = 0;

/// 原始系统调用（ABI 与 `libmorion` 相同：编号 rax、参数 rdi/rsi/rdx）。
///
/// rdi/rsi/rdx 是入参寄存器且被内核改写，必须用 inout 声明；rcx/r11 被 `syscall` 指令
/// 本身改写，r8/r9/r10 是 caller-saved 且内核不保存，故一并声明为 clobber。
unsafe fn raw_syscall(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") n => ret,
        inout("rdi") a1 => _,
        inout("rsi") a2 => _,
        inout("rdx") a3 => _,
        lateout("rcx") _,
        lateout("r11") _,
        lateout("r8") _,
        lateout("r9") _,
        lateout("r10") _,
        options(nostack)
    );
    ret
}

/// 把内核错误码翻译成可读原因。
fn error_reason(e: u64) -> &'static str {
    match e {
        ENCLAVE_ERR_NO_DEVICE => "无设备 / 选择不匹配",
        ENCLAVE_ERR_NO_CAP => "缺少 Mmio 能力",
        ENCLAVE_ERR_BAD_WINDOW => "窗口非法或越界",
        ENCLAVE_ERR_UNSUPPORTED => "IOMMU 不可用，保持恒等映射",
        ENCLAVE_ERR_CONFLICT => "BAR 窗口冲突",
        ENCLAVE_ERR_ALREADY_OWNED => "设备已被其它域独占",
        ENCLAVE_ERR_NO_IOVA => "IOVA 分配器窗口耗尽",
        _ => "未知错误",
    }
}

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

    // 2. 申请窗口：IOVA 传 0 = 由内核的每域位图分配器选址（4 KiB 粒度、64 MiB 窗口）。
    //    BAR 映射与设备上下文切换都在内核完成。
    let ret = unsafe { raw_syscall(SYS_ENCLAVE_MAP, DEVICE_SELF, 0, grant.dma_bytes) };
    if ret >= ENCLAVE_ERR_NO_IOVA {
        print("enclave: 申请窗口失败 err=");
        print_u64(ret);
        print(" (");
        print(error_reason(ret));
        println(")");
        return;
    }
    let iova = ret;
    print("enclave: 窗口已建立 iova=0x");
    print_hex(iova);
    print(" bytes=0x");
    print_hex(grant.dma_bytes);
    println("");

    // 3. 读寄存器：BAR 是非缓存映射，必须 volatile 读。
    let value = mmio::rd32(grant.bar_vaddr + REG_OFFSET);
    print("enclave: BAR0[0]=0x");
    print_hex(value as u64);
    println("");

    // 4. 解绑：恢复默认恒等映射、撤 IOVA 叶项并归还自动分配的 IOVA。
    let detached = unsafe { raw_syscall(SYS_ENCLAVE_DETACH, DEVICE_SELF, 0, 0) };
    if detached == 1 {
        println("enclave: detach 成功，设备已归还恒等映射");
    } else {
        print("enclave: detach 失败 err=");
        print_u64(detached);
        print(" (");
        print(error_reason(detached));
        println(")");
    }
    println("enclave: 完成，退出");
}
