//! ACPI 表解析（驱动路线 **E1a**）—— 目前只用一条路径：**RSDP → XSDT/RSDT → DMAR**。
//!
//! 内核此前完全不碰 ACPI（LAPIC / PCI 都走固定的经典 MMIO 与配置端口，见 `arch/apic.rs`、
//! `arch/pci.rs`）。DMAR 是第一个**必须**从固件描述里读出来的东西：它是 Intel VT-d 的能力表，
//! 给出 **DRHD**（DMA Remapping Hardware Unit：重映射单元的 MMIO 寄存器基址 + 它管辖的 PCI
//! 设备范围）与 **RMRR**（保留内存区），是 E1b/E1c 建重映射域、做 DMA 越界拒绝的依据。
//!
//! 读的物理内存都在内核**恒等映射**的前 4 GiB 内（RSDP/XSDT/DMAR 都由固件放在低 4 GiB），
//! 每处访问前都过 `memory::paging::is_identity_mapped`，固件给出畸形表时只**降级**（返回
//! 空摘要）而不 panic。ACPI 表所在的物理页是 `ACPI_RECLAIM` 类型，不在帧分配器的空闲池里，
//! 故把读到的字节切片当作 `'static` 是安全的（与 `bootinfo::service_modules` 同理）。
//!
//! E1a 只**取证**（解析 + 打印），不写重映射单元的寄存器 —— 那是 E1b。

use crate::memory::paging::is_identity_mapped;

/// DMAR 里一个 **DRHD**（DMA Remapping Hardware Unit Definition）的摘要。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Drhd {
    /// PCI 段号（一般 0）。
    pub segment: u16,
    /// `flags` bit 0 = `INCLUDE_PCI_ALL`：该单元管辖**所有** PCI 设备（设备范围表是空的）。
    pub include_all: bool,
    /// 该单元 MMIO 寄存器块的物理基址（E1b 往这里写根表 / 上下文表指针）。
    pub reg_base: u64,
    /// **设备范围**（Device Scope）条目数。
    pub scope_count: u16,
}

/// DMAR 探测结果。
///
/// `found == false`（固件没给 RSDP / 没给 DMAR，例如 QEMU 未开 `intel-iommu=on`）时其余字段
/// 全部为 0 —— 调用方据此打"IOMMU 不存在"并让整条驱动路线照常走（DMA 直通物理地址）。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DmarSummary {
    pub found: bool,
    /// 表头 `length` 字段声明的字节数。
    pub table_len: u32,
    /// ACPI 表校验和（整表各字节和为 0）是否通过。
    pub checksum_ok: bool,
    /// `Host Address Width`（DMA 地址位宽 - 1，如 0x2F = 48 位）。
    pub host_address_width: u8,
    pub drhd_count: u32,
    /// RMRR（保留内存区，type 1）条目数。
    pub rmrr_count: u32,
    /// 第一个 DRHD（`drhd_count > 0` 时有效）。
    pub first_drhd: Drhd,
}

// ---------------------------------------------------------------------------
// 小端读 (调用方保证已做长度检查)
// ---------------------------------------------------------------------------

fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// ACPI 表的校验和：整表各字节按字节相加必须为 0（含校验和字段本身）。
fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

/// ACPI 表头固定长度（`signature`..`creator_revision`）。
const ACPI_HEADER_LEN: usize = 36;

/// DMAR 表**固定部分**的长度：ACPI 表头 36B + `Host Address Width`(1) + `Flags`(1) +
/// `Reserved`(10) = **48B**；重映射结构从偏移 48 才开始。
///
/// ⚠️ 这里不是 36：DMAR 在 ACPI 表头后还有 12 字节自己的字段，漏掉就会把
/// `Host Address Width` 当成第一条结构的 `type` —— 表现为"表找到了但 DRHD/RMRR 全 0"。
const DMAR_STRUCTS_OFF: usize = 48;

// ---------------------------------------------------------------------------
// DMAR 解析（纯函数 —— 只看字节，便于单测）
// ---------------------------------------------------------------------------

/// 解析 DMAR 表字节。
///
/// 结构（ACPI 6.x 5.2.7 / VT-d）：固定部分 48 字节（ACPI 表头 36 + `Host Address Width` 1 +
/// `Flags` 1 + `Reserved` 10），之后是一串**重映射结构**，每条 `type(u16) length(u16) ...`；
/// type 0 = DRHD、1 = RMRR、2 = ATSR、3 = SATC。DRHD 的专属头是 16 字节
/// （`flags u8 / reserved u8 / segment u16 / reg_base u64`），其后每 8 字节一个设备范围条目。
pub fn parse_dmar(bytes: &[u8]) -> DmarSummary {
    let mut s = DmarSummary::default();
    if bytes.len() < DMAR_STRUCTS_OFF || !bytes.starts_with(b"DMAR") {
        return s;
    }
    let len = rd32(bytes, 4) as usize;
    // 表长必须自洽（>= 固定部分）且不超出我们实际读到的字节数。
    if len < DMAR_STRUCTS_OFF || len > bytes.len() {
        return s;
    }
    let t = &bytes[..len];
    s.found = true;
    s.table_len = len as u32;
    s.checksum_ok = checksum_ok(t);
    s.host_address_width = t[36];

    let mut off = DMAR_STRUCTS_OFF;
    while off + 4 <= len {
        let ty = rd16(t, off);
        let slen = rd16(t, off + 2) as usize;
        // 每条至少 4 字节，且不得越出表尾（否则视为畸形，停止扫描）。
        if slen < 4 || off + slen > len {
            break;
        }
        match ty {
            0 => {
                // DRHD：专属头 16 字节 + N*8 字节设备范围。
                if slen >= 16 {
                    let drhd = Drhd {
                        segment: rd16(t, off + 6),
                        include_all: t[off + 4] & 1 != 0,
                        reg_base: rd64(t, off + 8),
                        scope_count: ((slen - 16) / 8) as u16,
                    };
                    if s.drhd_count == 0 {
                        s.first_drhd = drhd;
                    }
                    s.drhd_count += 1;
                }
            }
            1 => s.rmrr_count += 1,
            _ => {}
        }
        off += slen;
    }
    s
}

// ---------------------------------------------------------------------------
// 物理内存遍历（恒等映射内）
// ---------------------------------------------------------------------------

/// 用表头里的 `length` 字段把一个恒等映射内的物理地址读成 ACPI 表切片。
///
/// 三重校验：地址非 0 且在恒等映射内、前 36 字节可达、签名为 `sig`、声明长度自洽且整段
/// 可达。任一不过返回 `None`。
fn table_at(paddr: u64, sig: &[u8; 4]) -> Option<&'static [u8]> {
    if paddr == 0 || !is_identity_mapped(paddr, ACPI_HEADER_LEN as u64) {
        return None;
    }
    // SAFETY: 上面已确认这 36 字节落在恒等映射内；ACPI 表所在页是 `ACPI_RECLAIM`，
    // 不在帧分配器空闲池里，故这块内存在本函数的 `'static` 生命周期内不会被复用。
    let hdr = unsafe { core::slice::from_raw_parts(paddr as *const u8, ACPI_HEADER_LEN) };
    if !hdr.starts_with(sig) {
        return None;
    }
    let len = rd32(hdr, 4) as u64;
    if len < ACPI_HEADER_LEN as u64 || !is_identity_mapped(paddr, len) {
        return None;
    }
    // SAFETY: 同上，且 `len` 已确认整段在恒等映射内。
    Some(unsafe { core::slice::from_raw_parts(paddr as *const u8, len as usize) })
}

/// 从 RSDP 取出根表（`revision >= 2` 用 **XSDT**，否则回退 **RSDT**）。
fn xsdt_or_rsdt(rsdp: u64) -> Option<(u64, bool)> {
    // RSDP 前 20 字节自带校验和（覆盖 signature..rsdt_address）。
    if rsdp == 0 || !is_identity_mapped(rsdp, 20) {
        return None;
    }
    // SAFETY: 已确认 20 字节在恒等映射内；RSDP 位于固件保留区，不会被复用。
    let h = unsafe { core::slice::from_raw_parts(rsdp as *const u8, 20) };
    if !h.starts_with(b"RSD PTR ") || !checksum_ok(h) {
        return None;
    }
    let revision = h[15];
    if revision >= 2 && is_identity_mapped(rsdp, ACPI_HEADER_LEN as u64) {
        // SAFETY: 同上。
        let h36 = unsafe { core::slice::from_raw_parts(rsdp as *const u8, ACPI_HEADER_LEN) };
        let xsdt = rd64(h36, 24);
        if xsdt != 0 {
            return Some((xsdt, true));
        }
    }
    let rsdt = rd32(h, 16) as u64;
    if rsdt == 0 {
        None
    } else {
        Some((rsdt, false))
    }
}

/// 遍历 XSDT/RSDT，找签名 `DMAR` 的表。
fn find_dmar(rsdp: u64) -> Option<&'static [u8]> {
    let (root, is_xsdt) = xsdt_or_rsdt(rsdp)?;
    let root_sig: &[u8; 4] = if is_xsdt { b"XSDT" } else { b"RSDT" };
    let table = table_at(root, root_sig)?;
    // 表头之后是一串"其他表"的物理地址（XSDT 每项 8 字节，RSDT 每项 4 字节）。
    let ent = if is_xsdt { 8 } else { 4 };
    let count = (table.len() - ACPI_HEADER_LEN) / ent;
    for i in 0..count {
        let off = ACPI_HEADER_LEN + i * ent;
        let paddr = if is_xsdt {
            rd64(table, off)
        } else {
            rd32(table, off) as u64
        };
        if paddr == 0 {
            continue;
        }
        if let Some(t) = table_at(paddr, b"DMAR") {
            return Some(t);
        }
    }
    None
}

/// 探测 DMAR（E1a）。引导器没给 RSDP / 固件没给 DMAR 时返回 `found = false` 的空摘要。
pub fn probe_dmar() -> DmarSummary {
    let rsdp = crate::bootinfo::get().rsdp_addr();
    match find_dmar(rsdp) {
        Some(bytes) => parse_dmar(bytes),
        None => DmarSummary::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put16(b: &mut [u8], off: usize, v: u16) {
        b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// 造一张最小合法 DMAR：固定部分 48B + 1 个 DRHD（INCLUDE_PCI_ALL，1 个设备范围）
    /// + 1 个 RMRR（16B）。
    fn synth_dmar() -> [u8; 48 + 24 + 16] {
        let mut t = [0u8; 48 + 24 + 16];
        let total = t.len() as u32;
        t[0..4].copy_from_slice(b"DMAR");
        put32(&mut t, 4, total);
        t[8] = 1; // revision
        t[36] = 0x2F; // Host Address Width (48 位 DMA 地址)
                      // DRHD: type 0, len 24, flags=1(INCLUDE_PCI_ALL), segment=0, reg_base=0xFED9_0000, 1 scope
        put16(&mut t, 48, 0);
        put16(&mut t, 50, 24);
        t[52] = 1;
        put16(&mut t, 54, 0);
        put64(&mut t, 56, 0xFED9_0000);
        // RMRR: type 1, len 16（内容不解析）
        put16(&mut t, 72, 1);
        put16(&mut t, 74, 16);
        // 补校验和（校验和字段在偏移 9）
        let sum = t
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 9)
            .fold(0u8, |a, (_, &b)| a.wrapping_add(b));
        t[9] = 0u8.wrapping_sub(sum);
        t
    }

    #[test]
    fn parses_drhd_and_rmrr() {
        let t = synth_dmar();
        assert!(checksum_ok(&t), "合成表校验和应为 0");
        let s = parse_dmar(&t);
        assert!(s.found && s.checksum_ok);
        assert_eq!(s.table_len, t.len() as u32);
        assert_eq!(s.host_address_width, 0x2F);
        assert_eq!(s.drhd_count, 1);
        assert_eq!(s.rmrr_count, 1);
        let d = s.first_drhd;
        assert!(d.include_all);
        assert_eq!(d.segment, 0);
        assert_eq!(d.reg_base, 0xFED9_0000);
        assert_eq!(d.scope_count, 1); // (24 - 16) / 8
    }

    #[test]
    fn rejects_malformed_dmar() {
        // 签名不对。
        let mut t = synth_dmar();
        t[0] = b'X';
        assert!(!parse_dmar(&t).found);
        // 声明长度越界（> 实到字节数）。
        let mut t = synth_dmar();
        put32(&mut t, 4, 9999);
        assert!(!parse_dmar(&t).found);
        // 结构里出现"长度 < 4"的畸形条目：停止扫描，但表本身仍算找到。
        let mut t = synth_dmar();
        put16(&mut t, 50, 2); // DRHD 条目声明长度 2
        let s = parse_dmar(&t);
        assert!(s.found);
        assert_eq!(s.drhd_count, 0);
    }
}
