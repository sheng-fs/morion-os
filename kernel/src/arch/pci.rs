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

/// 读取设备 BAR0 (支持 64 位 MMIO BAR), 返回其物理基址。
pub fn read_bar0(bus: u8, dev: u8, func: u8) -> Option<u64> {
    let low = config_read_dword(bus, dev, func, 0x10);
    // bit0=0 表示内存空间 BAR (MMIO); bit0=1 表示 I/O 空间, NVMe 不支持。
    if low & 0x1 != 0 {
        return None;
    }
    let bar_type = (low >> 1) & 0x3;
    if bar_type == 0b10 {
        // 64 位 BAR: 与下一 dword 拼接。
        let high = config_read_dword(bus, dev, func, 0x14);
        Some(((high as u64) << 32) | ((low & 0xFFFF_FFF0) as u64))
    } else {
        Some((low & 0xFFFF_FFF0) as u64)
    }
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
