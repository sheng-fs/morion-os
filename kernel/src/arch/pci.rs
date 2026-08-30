//! PCI 配置空间枚举 (type 1, I/O 端口 0xCF8/0xCFC)
//!
//! 扫描 bus 0..=255 / device 0..=31 / function 0..=7, 枚举所有存在的
//! PCI(e) 设备; 用于后续定位 NVMe 控制器 (class 01:08:02) 并读取其 BAR0。

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// 配置地址端口 (选择 bus/device/function/offset)。
const CONFIG_ADDR: u16 = 0xCF8;
/// 配置数据端口 (读写所选 32 位)。
const CONFIG_DATA: u16 = 0xCFC;

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
