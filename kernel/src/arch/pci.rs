//! PCI 配置空间枚举 (type 1, I/O 端口 0xCF8/0xCFC)
//!
//! 扫描 bus 0..=255 / device 0..=31 / function 0..=7, 枚举所有存在的
//! PCI(e) 设备; 用于后续定位 NVMe 控制器 (class 01:08:02) 并读取其 BAR0。
//! 另含能力链表遍历与 MSI-X 的定位/使能 (阶段 4: MSI/MSI-X 中断)。

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// 配置地址端口 (选择 bus/device/function/offset)。
const CONFIG_ADDR: u16 = 0xCF8;
/// 配置数据端口 (读写所选 32 位)。
const CONFIG_DATA: u16 = 0xCFC;

/// 配置空间寄存器偏移: 命令寄存器 (16 位)。
const REG_COMMAND: u8 = 0x04;
/// 状态寄存器 (16 位); bit4 = 支持能力链表。
const REG_STATUS: u8 = 0x06;
/// 能力链表头指针 (8 位, bits 7:2)。
const REG_CAP_PTR: u8 = 0x34;
/// 命令寄存器 bit10: 禁用 INTx 中断。
const CMD_INTX_DISABLE: u16 = 1 << 10;
/// 状态寄存器 bit4: 能力链表存在。
const STATUS_CAP_LIST: u16 = 1 << 4;
/// 能力 ID: MSI-X。
const CAP_ID_MSIX: u8 = 0x11;

/// 枚举到的一台 PCI 设备。
#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub progif: u8,
}

/// 读取 PCI 配置空间 32 位 (type 1 访问)。
pub fn config_read_dword(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDR);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        addr_port.write(addr);
        data_port.read()
    }
}

/// 读取设备 vendor/device id; 返回 None 表示该位置无设备。
fn read_ids(bus: u8, dev: u8, func: u8) -> Option<(u16, u16)> {
    let vd = config_read_dword(bus, dev, func, 0x00);
    let vendor = (vd & 0xFFFF) as u16;
    let device = (vd >> 16) as u16;
    if vendor == 0xFFFF {
        None
    } else {
        Some((vendor, device))
    }
}

/// 扫描并返回所有存在的 PCI 设备。
pub fn enumerate() -> Vec<PciDevice> {
    let mut devices = Vec::new();

    for bus in 0..256usize {
        for dev in 0..32usize {
            let (bus, dev) = (bus as u8, dev as u8);
            if read_ids(bus, dev, 0).is_none() {
                continue;
            }

            // header type bit7 = 1 表示多设备功能 (func 1..7 可能另有设备)。
            let header = config_read_dword(bus, dev, 0, 0x0C);
            let multifunction = (header >> 16) & 0x80 != 0;
            let max_func = if multifunction { 8u8 } else { 1u8 };

            for func in 0..max_func {
                let (vendor, device) = match read_ids(bus, dev, func) {
                    Some(vd) => vd,
                    None => continue,
                };
                let cc = config_read_dword(bus, dev, func, 0x08);
                let class = ((cc >> 24) & 0xFF) as u8;
                let subclass = ((cc >> 16) & 0xFF) as u8;
                let progif = ((cc >> 8) & 0xFF) as u8;

                devices.push(PciDevice {
                    bus,
                    dev,
                    func,
                    vendor,
                    device,
                    class,
                    subclass,
                    progif,
                });
            }
        }
    }

    devices
}

/// 在枚举结果中查找 NVMe 控制器 (class 01:08:02), 返回其 BAR0 物理地址。
pub fn find_nvme(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class == 0x01 && d.subclass == 0x08 && d.progif == 0x02 {
            if let Some(bar0) = read_bar0(d.bus, d.dev, d.func) {
                return Some((d.bus, d.dev, d.func, bar0));
            }
        }
    }
    None
}

/// 在枚举结果中查找 **virtio 网卡** (网络控制器 class `02`, vendor `1AF4`), 返回其
/// PCI 位置与 **virtio-modern 配置 BAR (BAR4)** 的物理基址。
///
/// 只认 virtio: 别的网卡本内核没有驱动, 认出来也无用。virtio-modern 把
/// common cfg / notify / device cfg / ISR 都排在 **BAR4** (QEMU 给的是 64 位可预取 MMIO),
/// 而 legacy 用的是 I/O 空间 BAR0 —— 本内核只支持 MMIO, 故只走 modern 路径。
/// QEMU `virtio-net-pci` 默认 (transitional) 报 legacy id `1000`, `disable-legacy=on`
/// 报 modern id `1041`; 两者都带 BAR4, 故两个 id 都认。
pub fn find_net(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class != 0x02 || d.vendor != 0x1AF4 {
            continue;
        }
        if d.device != 0x1041 && d.device != 0x1000 {
            continue;
        }
        if let Some(bar4) = read_bar(d.bus, d.dev, d.func, 4) {
            return Some((d.bus, d.dev, d.func, bar4));
        }
    }
    None
}

/// 在枚举结果中查找 **AHCI/SATA 控制器** (大容量存储控制器 class `01`, subclass `06`,
/// prog-if `01` = AHCI 1.0), 返回其 PCI 位置与 **ABAR (BAR5)** 的物理基址。
///
/// 只认 AHCI 编程接口 (`01:06:01`): 老式 IDE (prog-if `80`) 与 RAID (`04`) 不在此列。
/// ABAR 是 MMIO 窗口 (通用主机控制 + 每端口寄存器), 大小通常 8 KiB。
pub fn find_ahci(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class != 0x01 || d.subclass != 0x06 || d.progif != 0x01 {
            continue;
        }
        if let Some(bar5) = read_bar(d.bus, d.dev, d.func, 5) {
            return Some((d.bus, d.dev, d.func, bar5));
        }
    }
    None
}

/// 在枚举结果中查找 **xHCI (USB 3.x) 控制器** (串行总线控制器 class `0C`, subclass `03`,
/// prog-if `30` = xHCI), 返回其 PCI 位置与 **BAR0** 的物理基址。
///
/// prog-if `30` 天然滤掉 q35 上的 UHCI (`00`) / EHCI (`20`) —— 本内核只驱动 xHCI。
/// BAR0 是 64 位 MMIO 寄存器窗口 (QEMU `qemu-xhci` 约 4 页)。
pub fn find_xhci(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class != 0x0C || d.subclass != 0x03 || d.progif != 0x30 {
            continue;
        }
        if let Some(bar0) = read_bar0(d.bus, d.dev, d.func) {
            return Some((d.bus, d.dev, d.func, bar0));
        }
    }
    None
}

/// 在枚举结果中查找 **Intel e1000e (82574L)** 网卡 (网络控制器 class `02`, subclass `00`,
/// vendor `8086`, device `10D3`), 返回其 PCI 位置与 **BAR0** 的物理基址。
///
/// N9 的第二台真网卡: 与 [`find_net`] 并列, 证明"驱动 ≠ 栈"——同一套帧级 IPC, 换成完全不同的
/// 硬件寄存器模型 (MMIO 描述符环, 无 virtio 能力链表)。只认 82574L (`10D3`), 其它 Intel 网卡
/// 本内核没有驱动。
pub fn find_e1000e(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class != 0x02 || d.vendor != 0x8086 {
            continue;
        }
        if d.device != 0x10D3 {
            continue;
        }
        if let Some(bar0) = read_bar0(d.bus, d.dev, d.func) {
            return Some((d.bus, d.dev, d.func, bar0));
        }
    }
    None
}

/// 在枚举结果中查找 **virtio-blk** (存储控制器 class `01`, vendor `1AF4`, device `1042`/`1001`),
/// 返回其 PCI 位置与 **virtio-modern 配置 BAR (BAR4)** 的物理基址。
///
/// 与 [`find_net`] 同款: 只认 virtio, 只走 modern (MMIO) 路径 —— 新驱动因此仍不改内核设备逻辑。
pub fn find_virtio_blk(devices: &[PciDevice]) -> Option<(u8, u8, u8, u64)> {
    for d in devices {
        if d.class != 0x01 || d.vendor != 0x1AF4 {
            continue;
        }
        if d.device != 0x1042 && d.device != 0x1001 {
            continue;
        }
        if let Some(bar4) = read_bar(d.bus, d.dev, d.func, 4) {
            return Some((d.bus, d.dev, d.func, bar4));
        }
    }
    None
}

/// 读取设备第 `index` 根 BAR (`0..=5`) 的物理基址; `None` = 该 BAR 不存在或为 I/O 空间
/// (本内核只驱动 MMIO 设备)。
pub fn read_bar(bus: u8, dev: u8, func: u8, index: u8) -> Option<u64> {
    let off = 0x10 + index * 4;
    let low = config_read_dword(bus, dev, func, off);
    // bit0=1: I/O 空间 BAR (MMIO 驱动用不到)。
    if low & 0x1 != 0 {
        return None;
    }
    let addr = if (low >> 1) & 0x3 == 0b10 {
        // 64 位 BAR: 与下一 dword 拼接。基址可能高于 4 GiB, 此时低 dword 只剩标志位,
        // 故**不能**拿低 dword 判"未实现"。
        let high = config_read_dword(bus, dev, func, off + 4);
        ((high as u64) << 32) | ((low & 0xFFFF_FFF0) as u64)
    } else {
        (low & 0xFFFF_FFF0) as u64
    };
    // 未实现的 BAR 读回 0 (基址为 0 视为不存在)。
    if addr == 0 {
        return None;
    }
    Some(addr)
}

/// 读取设备 BAR0 (支持 64 位 MMIO BAR), 返回其物理基址。
pub fn read_bar0(bus: u8, dev: u8, func: u8) -> Option<u64> {
    read_bar(bus, dev, func, 0)
}

// ---------------------------------------------------------------------------
// 配置空间读写 (含 8/16 位) 与能力链表
// ---------------------------------------------------------------------------

/// 读配置空间 16 位 (按 dword 读取后取对应半字)。
pub fn config_read_word(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let dword = config_read_dword(bus, dev, func, offset & !0x3);
    ((dword >> (u32::from(offset & 0x2) * 8)) & 0xFFFF) as u16
}

/// 读配置空间 8 位。
pub fn config_read_byte(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let dword = config_read_dword(bus, dev, func, offset & !0x3);
    ((dword >> (u32::from(offset & 0x3) * 8)) & 0xFF) as u8
}

/// 写配置空间 32 位 (type 1 访问)。
pub fn config_write_dword(bus: u8, dev: u8, func: u8, offset: u8, value: u32) {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDR);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        addr_port.write(addr);
        data_port.write(value);
    }
}

/// 读-改-写一个 16 位配置寄存器 (`set` 位置 1, `clear` 位置 0), 返回改写前的值。
fn config_update_word(bus: u8, dev: u8, func: u8, offset: u8, clear: u16, set: u16) -> u16 {
    let dword_off = offset & !0x3;
    let dword = config_read_dword(bus, dev, func, dword_off);
    let shift = u32::from(offset & 0x2) * 8;
    let cur = ((dword >> shift) & 0xFFFF) as u16;
    let new = (cur & !clear) | set;
    let merged = (dword & !(0xFFFFu32 << shift)) | ((new as u32) << shift);
    config_write_dword(bus, dev, func, dword_off, merged);
    cur
}

/// MSI-X 能力 (capability id 0x11) 的关键字段。
#[derive(Clone, Copy, Debug)]
pub struct MsixCap {
    /// 能力结构在配置空间中的偏移 (使能位就在 `cap_ptr + 2`)。
    pub cap_ptr: u8,
    /// MSI-X 表相对其所属 BAR 的字节偏移 (低 3 位是 BIR)。
    pub table_offset: u32,
    /// MSI-X 表所在 BAR 的编号 (BIR)。
    pub table_bir: u8,
    /// 表项数 (消息控制寄存器 bits 10:0 加 1)。
    pub table_size: u16,
}

/// 遍历能力链表, 返回第一个 MSI-X 能力。
pub fn find_msix(bus: u8, dev: u8, func: u8) -> Option<MsixCap> {
    if config_read_word(bus, dev, func, REG_STATUS) & STATUS_CAP_LIST == 0 {
        return None;
    }
    let mut ptr = config_read_byte(bus, dev, func, REG_CAP_PTR) & 0xFC;
    // 链表节点数有限 (PCI 规范给的上限是 48), 循环上限同时挡住固件给出的环。
    for _ in 0..48 {
        // 能力结构的偏移必须 >= 0x40 (0x00..0x3F 是标准头)。
        if ptr < 0x40 {
            return None;
        }
        if config_read_byte(bus, dev, func, ptr) == CAP_ID_MSIX {
            let ctrl = config_read_word(bus, dev, func, ptr + 2);
            let table = config_read_dword(bus, dev, func, ptr + 4);
            return Some(MsixCap {
                cap_ptr: ptr,
                table_offset: table & !0x7,
                table_bir: (table & 0x7) as u8,
                table_size: (ctrl & 0x7FF) + 1,
            });
        }
        let next = config_read_byte(bus, dev, func, ptr + 1) & 0xFC;
        if next == 0 {
            return None;
        }
        ptr = next;
    }
    None
}

/// 置命令寄存器的 INTx 禁用位 (启用 MSI/MSI-X 前的标准动作: 避免两者同时触发)。
pub fn disable_intx(bus: u8, dev: u8, func: u8) {
    config_update_word(bus, dev, func, REG_COMMAND, 0, CMD_INTX_DISABLE);
}

/// 打开 MSI-X: 置消息控制寄存器 bit15 (Enable) 并清 bit14 (Function Mask)。
///
/// 调用顺序要求: 先关 INTx、先把表项写好, 再调本函数 —— 置位后设备随时可能投递中断。
pub fn enable_msix(bus: u8, dev: u8, func: u8, cap_ptr: u8) {
    config_update_word(bus, dev, func, cap_ptr + 2, 0x4000, 0x8000);
}
