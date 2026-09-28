//! ELF64 解析与校验 —— 可执行文件加载的**信任边界**
//!
//! 只认本项目的用户程序目标产出（`user/x86_64-morion-user.json`）那一种镜像：
//! 64 位、小端、`ET_EXEC`（非 PIE、静态，链接在 `USER_SPACE_BASE`）、`EM_X86_64`。
//!
//! ⚠️ `SYS_SPAWN_ELF` 的镜像字节**完全由用户态提供**（可以任何东西），所以这一层的
//! 每个字段都是"先校验、再使用"：越界、重叠、超大、非用户空间一律拒绝，
//! 且不分配任何资源、不 panic —— 调用方拿到 `None` 就干净地失败。

use crate::memory::paging::{USER_SPACE_BASE, USER_SPACE_END};

/// 最多接受的 `PT_LOAD` 段数（正常产物 3~5 段；给足余量但不允许无限）。
pub const MAX_SEGMENTS: usize = 32;

/// 程序头表项长度（ELF64 固定 56 字节）。
const PHENT_SIZE: u64 = 56;

/// 段权限位（`p_flags`）。
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

/// 一个需要载入内存的段（`PT_LOAD`）。
#[derive(Clone, Copy)]
pub struct Segment {
    /// 段在文件中的起始偏移。
    pub offset: u64,
    /// 段应载入的虚拟地址。
    pub vaddr: u64,
    /// 段在文件中的字节数（`p_filesz`）。
    pub filesz: u64,
    /// 段在内存中占的字节数（`p_memsz`，≥ `filesz`，差额即 `.bss`）。
    pub memsz: u64,
    /// 段权限位（`p_flags`，见 `PF_X` / `PF_W`）。
    pub flags: u32,
}

/// 解析结果：入口地址 + 全部 `PT_LOAD` 段。
pub struct Image {
    pub entry: u64,
    pub segments: [Segment; MAX_SEGMENTS],
    pub count: usize,
}

impl Image {
    /// 迭代已解析的段。
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// 小端读取辅助（越界返回 `None`，绝不 panic）。
fn u16le(bytes: &[u8], off: usize) -> Option<u16> {
    let s = bytes.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32le(bytes: &[u8], off: usize) -> Option<u32> {
    let s = bytes.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64le(bytes: &[u8], off: usize) -> Option<u64> {
    let s = bytes.get(off..off + 8)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

/// 解析并校验一个 ELF64 `ET_EXEC` 镜像。
///
/// 返回 `None` 表示镜像非法（调用方直接失败，不做任何清理 —— 本函数不分配资源）。
pub fn parse(bytes: &[u8]) -> Option<Image> {
    // --- ELF 头 ---
    if bytes.len() < 64 {
        return None;
    }
    if bytes.get(..4)? != b"\x7fELF" {
        return None;
    }
    if bytes[4] != 2 {
        return None; // ELFCLASS64
    }
    if bytes[5] != 1 {
        return None; // ELFDATA2LSB
    }
    if bytes[6] != 1 {
        return None; // EV_CURRENT
    }
    // e_type = ET_EXEC(2)：非 PIE 的固定基址可执行文件；ET_DYN 需要重定位，本加载器不做。
    if u16le(bytes, 16)? != 2 {
        return None;
    }
    // e_machine = EM_X86_64(62)
    if u16le(bytes, 18)? != 62 {
        return None;
    }

    let entry = u64le(bytes, 24)?;
    let phoff = u64le(bytes, 32)?;
    let phentsize = u16le(bytes, 54)? as u64;
    let phnum = u16le(bytes, 56)? as usize;

    if phentsize != PHENT_SIZE {
        return None;
    }
    if phnum == 0 || phnum > MAX_SEGMENTS {
        return None;
    }
    // 程序头表整体必须落在镜像内。
    let ph_end = phoff.checked_add(phentsize.checked_mul(phnum as u64)?)?;
    if ph_end > bytes.len() as u64 {
        return None;
    }

    // 入口必须落在用户空间（否则等于允许"跳进内核"）。
    if !in_user_space(entry) {
        return None;
    }

    // --- 程序头 ---
    let mut image = Image {
        entry,
        segments: [Segment {
            offset: 0,
            vaddr: 0,
            filesz: 0,
            memsz: 0,
            flags: 0,
        }; MAX_SEGMENTS],
        count: 0,
    };

    for i in 0..phnum {
        let ph = (phoff + (i as u64) * phentsize) as usize;
        let p_type = u32le(bytes, ph)?;
        if p_type != 1 {
            continue; // 只加载 PT_LOAD
        }
        let p_flags = u32le(bytes, ph + 4)?;
        let p_offset = u64le(bytes, ph + 8)?;
        let p_vaddr = u64le(bytes, ph + 16)?;
        let p_filesz = u64le(bytes, ph + 32)?;
        let p_memsz = u64le(bytes, ph + 40)?;

        // W^X: 一个段不允许同时可写、可执行 —— 这种镜像在加载前就拒绝,
        // 而不是映出一页 RWX (页级 W^X 的镜像侧前提, 见 `exec::map_image`)。
        if p_flags & PF_W != 0 && p_flags & PF_X != 0 {
            return None;
        }

        // 文件内容必须整段落在镜像里（`p_filesz == 0` 的纯 .bss 段除外）。
        let file_end = p_offset.checked_add(p_filesz)?;
        if file_end > bytes.len() as u64 {
            return None;
        }
        // 内存占用必须是 `filesz ≤ memsz`（差额由加载器补零）。
        if p_filesz > p_memsz {
            return None;
        }
        if p_memsz == 0 {
            continue;
        }
        // 段整体必须落在用户空间内（含末页越界的检查）。
        if !in_user_space(p_vaddr) {
            return None;
        }
        let mem_end = p_vaddr.checked_add(p_memsz)?;
        if mem_end > USER_SPACE_END {
            return None;
        }

        image.segments[image.count] = Segment {
            offset: p_offset,
            vaddr: p_vaddr,
            filesz: p_filesz,
            memsz: p_memsz,
            flags: p_flags,
        };
        image.count += 1;
    }

    if image.count == 0 {
        return None;
    }
    // 入口必须在某个已载入段的范围内 —— 否则一开局就跳到没映射的地址上（#PF）。
    let entry_in_segment = image
        .segments()
        .iter()
        .any(|s| entry >= s.vaddr && entry < s.vaddr + s.memsz);
    if !entry_in_segment {
        return None;
    }
    Some(image)
}

/// 该虚拟地址是否落在用户空间（P4[1]）。
fn in_user_space(vaddr: u64) -> bool {
    crate::memory::paging::is_user_address(vaddr) && vaddr >= USER_SPACE_BASE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小可用的 ELF64 头 + 一个 PT_LOAD 段。
    fn sample() -> alloc::vec::Vec<u8> {
        let mut b = alloc::vec![0u8; 64 + 56 + 0x10];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        b[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        let entry = USER_SPACE_BASE + 0x1000;
        b[24..32].copy_from_slice(&entry.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let ph = 64;
        b[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        b[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R|PF_X
        b[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        b[ph + 16..ph + 24].copy_from_slice(&USER_SPACE_BASE.to_le_bytes()); // p_vaddr
        b[ph + 32..ph + 40].copy_from_slice(&0x10u64.to_le_bytes()); // p_filesz
        b[ph + 40..ph + 48].copy_from_slice(&0x2000u64.to_le_bytes()); // p_memsz (须覆盖入口)
        b
    }

    #[test]
    fn accepts_minimal_image() {
        let img = parse(&sample()).expect("应接受最小镜像");
        assert_eq!(img.count, 1);
        assert_eq!(img.entry, USER_SPACE_BASE + 0x1000);
        assert_eq!(img.segments()[0].filesz, 0x10);
        assert_eq!(img.segments()[0].memsz, 0x2000);
        // 段权限原样带出, 供 `exec::map_image` 决定页权限。
        assert_eq!(img.segments()[0].flags, PF_X | 4);
    }

    /// W^X 的镜像侧契约: 同时可写可执行的段一律拒绝。
    #[test]
    fn rejects_writable_executable_segment() {
        let mut bad = sample();
        let ph = 64;
        bad[ph + 4..ph + 8].copy_from_slice(&3u32.to_le_bytes()); // PF_W|PF_X
        assert!(parse(&bad).is_none(), "W+X 段应拒绝");

        // 只有 W (无 X) 与只有 X (无 W) 都应放行。
        let mut ok = sample();
        ok[ph + 4..ph + 8].copy_from_slice(&2u32.to_le_bytes()); // PF_W
        assert!(parse(&ok).is_some(), "纯可写段应接受");

        let mut ok = sample();
        ok[ph + 4..ph + 8].copy_from_slice(&1u32.to_le_bytes()); // PF_X
        assert!(parse(&ok).is_some(), "纯可执行段应接受");
    }

    #[test]
    fn rejects_bad_headers() {
        let good = sample();

        let mut bad = good.clone();
        bad[1] = b'X';
        assert!(parse(&bad).is_none(), "magic 不符应拒绝");

        let mut bad = good.clone();
        bad[4] = 1; // ELFCLASS32
        assert!(parse(&bad).is_none(), "非 64 位应拒绝");

        let mut bad = good.clone();
        bad[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        assert!(parse(&bad).is_none(), "ET_DYN 应拒绝");

        let mut bad = good.clone();
        bad[18..20].copy_from_slice(&40u16.to_le_bytes()); // EM_ARM
        assert!(parse(&bad).is_none(), "非 x86_64 应拒绝");

        let mut bad = good.clone();
        bad[56..58].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse(&bad).is_none(), "无程序头应拒绝");

        let mut bad = good.clone();
        bad[54..56].copy_from_slice(&40u16.to_le_bytes()); // 错误的 phentsize
        assert!(parse(&bad).is_none(), "phentsize 不符应拒绝");
    }

    #[test]
    fn rejects_out_of_range_fields() {
        // 文件内容越界（p_offset + p_filesz > len）
        let mut bad = sample();
        let ph = 64;
        bad[ph + 32..ph + 40].copy_from_slice(&0x1000u64.to_le_bytes());
        assert!(parse(&bad).is_none(), "文件内容越界应拒绝");

        // filesz > memsz
        let mut bad = sample();
        bad[ph + 32..ph + 40].copy_from_slice(&0x20u64.to_le_bytes());
        bad[ph + 40..ph + 48].copy_from_slice(&0x10u64.to_le_bytes());
        assert!(parse(&bad).is_none(), "filesz > memsz 应拒绝");

        // 段落在用户空间之外（内核地址）
        let mut bad = sample();
        bad[ph + 16..ph + 24].copy_from_slice(&0x1000u64.to_le_bytes());
        assert!(parse(&bad).is_none(), "段落在内核地址应拒绝");

        // 段越过用户空间上界（会溢出到 P4[2]）
        let mut bad = sample();
        bad[ph + 16..ph + 24].copy_from_slice(&USER_SPACE_END.to_le_bytes());
        bad[ph + 40..ph + 48].copy_from_slice(&0x2000u64.to_le_bytes());
        assert!(parse(&bad).is_none(), "段越过用户空间上界应拒绝");

        // 入口不在任何段内
        let mut bad = sample();
        bad[24..32].copy_from_slice(&(USER_SPACE_BASE + 0x9000).to_le_bytes());
        assert!(parse(&bad).is_none(), "入口不在段内应拒绝");

        // 截断的镜像
        let good = sample();
        assert!(parse(&good[..64]).is_none(), "截断程序头表应拒绝");
        assert!(parse(&[]).is_none(), "空镜像应拒绝");
    }

    #[test]
    fn accepts_layout_with_bss_only_segment() {
        // 第二个段 filesz = 0（纯 .bss）：应被接受并保留 memsz。
        let mut b = sample();
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b.resize(64 + 112 + 0x10, 0);
        let ph = 64 + 56;
        b[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        let vaddr = USER_SPACE_BASE + 0x2000;
        b[ph + 8..ph + 16].copy_from_slice(&0x10u64.to_le_bytes());
        b[ph + 16..ph + 24].copy_from_slice(&vaddr.to_le_bytes());
        b[ph + 32..ph + 40].copy_from_slice(&0u64.to_le_bytes());
        b[ph + 40..ph + 48].copy_from_slice(&0x1000u64.to_le_bytes());
        let img = parse(&b).expect("应接受带纯 .bss 段的镜像");
        assert_eq!(img.count, 2);
    }
}
