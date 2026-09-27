#!/usr/bin/env python3
"""生成终端点阵字库 `kernel/src/video/cjk.bin`（汉字 / 全角标点 / 非 ASCII 窄字形）。

## 为什么需要它

内核终端原本只有 8x16 的 ASCII 位图字体（`kernel/src/video/font.rs`），
`SYS_PUTS` 收到的是 **UTF-8** 字符串，字节逐个喂给 `draw_char` 时汉字（3 字节/字）
只会落到「不可打印」分支被丢掉 —— 于是中文直接不显示。汉字必须是 16x16 点阵
（8 像素宽的格子画不出可辨认的笔画），占 **2 个字符格**。

## 字库来源

GNU Unifont（https://unifoundry.com/unifont/）—— 唯一覆盖整个 Unicode BMP 的
**开源点阵**字体，天然是 8x16 / 16x16 两档宽度，正好对上本终端的字符格。
许可为 **GPLv2+ with the GNU font embedding exception** 与 **OFL-1.1** 双许可：
按 OFL-1.1 取用其位图子集嵌入本内核（MIT）不改变本仓库许可，Unifont 未声明
保留字体名（RFN），故子集化后仍可沿用原名引用；出处见本文件与
`docs/dev-reference.md`。只取位图、不取轮廓，且只取子集，产物 ≈ 0.28 MB。

## 字符集（保证「该显示的都能显示」）

1. **GB2312 全集**：Python 内建 `gb2312` 编解码表直接枚举（一级 3755 + 二级 3008
   汉字 + 682 个符号/全角字母），即中文环境下最常用的一档覆盖，无需下载任何码表。
2. **仓库源码里出现过的所有非 ASCII 字符**：扫 `kernel/src` / `user/src` / `boot/src`
   / `docs` / `README*.md`。这一条保证**本项目自己**打印或注释里用到的字符（如
   `·`、`—`、`‖`、`√`、各种箭头与方框绘制符）一个不缺，不受 GB2312 覆盖面限制。

## 产物格式（定长 37 字节记录，按码点升序 → 内核二分查找）

    偏移 0..4   码点 u32（大端）
    偏移 4      宽度：1 = 8x16 窄字形，2 = 16x16 宽字形（即该字符占的字符格数）
    偏移 5..37  16 行 x u16（大端），每行 bit15 为最左像素
                窄字形只用高字节（低字节为 0）

宽度**随字形一起存**，于是内核不必再维护一张 East Asian Width 表：排版的列数
直接来自字体数据本身（GB2312 里的 `·` 是窄的、全角 `，` 是宽的，各自都对得上）。

## 用法

    python3 scripts/gen-cjk-font.py --download          # 首次: 下载 hex 到 build/
    python3 scripts/gen-cjk-font.py                     # 用缓存重新生成
    python3 scripts/gen-cjk-font.py --hex <path>        # 指定 hex

生成物需要提交进仓库（构建不依赖网络与 Python）。
"""

import argparse
import os
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 与脚本同址的这些路径用于「仓库里出现过的非 ASCII 字符」这一步。
SCAN_PATHS = ["kernel/src", "user/src", "boot/src", "docs", "README.md", "README.en.md"]

RECORD = 37
UNIFONT_VERSION = "18.0.01"
UNIFONT_URL = (
    f"https://ftp.gnu.org/gnu/unifont/unifont-{UNIFONT_VERSION}/"
    f"unifont_all-{UNIFONT_VERSION}.hex.gz"
)


def download_hex(cache_dir):
    """下载 Unifont 的 hex（全平面）并解压，返回解压后的路径。"""
    os.makedirs(cache_dir, exist_ok=True)
    gz = os.path.join(cache_dir, f"unifont_all-{UNIFONT_VERSION}.hex.gz")
    hex_path = gz[:-3]
    if not os.path.exists(hex_path):
        if not os.path.exists(gz):
            print(f"[gen] 下载 {UNIFONT_URL}")
            subprocess.run(["curl", "-fsSL", "-o", gz, UNIFONT_URL], check=True)
        print(f"[gen] 解压 {gz}")
        subprocess.run(["gunzip", "-kf", gz], check=True)
    return hex_path


def gb2312_charset():
    """用 Python 内建 gb2312 编解码表枚举 GB2312 全集（无需外部码表）。

    区位码高字节 0xA1..0xF7、低字节 0xA1..0xFE；解不出的空位跳过。
    """
    chars = set()
    for hi in range(0xA1, 0xF8):
        for lo in range(0xA1, 0xFF):
            try:
                chars.add(bytes([hi, lo]).decode("gb2312"))
            except UnicodeDecodeError:
                pass
    return chars


def repo_charset():
    """扫仓库源码/文档，收集出现过的所有非 ASCII 字符。"""
    chars = set()
    for rel in SCAN_PATHS:
        path = os.path.join(REPO, rel)
        if os.path.isfile(path):
            files = [path]
        elif os.path.isdir(path):
            files = [
                os.path.join(root, name)
                for root, _dirs, names in os.walk(path)
                for name in names
            ]
        else:
            continue
        for f in files:
            try:
                with open(f, "r", encoding="utf-8") as fh:
                    text = fh.read()
            except (UnicodeDecodeError, OSError):
                continue
            chars.update(c for c in text if ord(c) > 0x7F)
    return chars


def parse_hex(path):
    """解析 Unifont hex → {码点: (宽度, 32 字节行数据)}。

    hex 行的字形数据有两种长度：32 个 hex 字符 = 16 字节 = **8x16 窄字形**（每行 1 字节），
    64 个 hex 字符 = 32 字节 = **16x16 宽字形**（每行 2 字节，大端）。其余（如 16 行以上
    的变体）本终端用不上，跳过并计数。
    """
    glyphs = {}
    skipped = 0
    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or ":" not in line:
                continue
            cp_hex, data_hex = line.split(":", 1)
            try:
                cp = int(cp_hex, 16)
                data = bytes.fromhex(data_hex)
            except ValueError:
                skipped += 1
                continue
            if len(data) == 16:  # 8x16：每行 1 字节, 放到 u16 高字节
                rows = bytearray()
                for b in data:
                    rows += bytes((b, 0))
                glyphs[cp] = (1, bytes(rows))
            elif len(data) == 32:  # 16x16：每行 2 字节, 原样
                glyphs[cp] = (2, data)
            else:
                skipped += 1
    return glyphs, skipped


def build_records(charset, glyphs):
    """按码点升序打包定长记录；返回 (blob, 缺失字符, 宽/窄计数)。"""
    missing = []
    wide = narrow = 0
    out = bytearray()
    for cp in sorted(ord(c) for c in charset):
        glyph = glyphs.get(cp)
        if glyph is None:
            missing.append(cp)
            continue
        width, rows = glyph
        if width == 2:
            wide += 1
        else:
            narrow += 1
        out += cp.to_bytes(4, "big")
        out.append(width)
        out += rows
    return bytes(out), missing, wide, narrow


def main():
    ap = argparse.ArgumentParser(description="生成内核终端点阵字库 cjk.bin")
    ap.add_argument("--hex", help="Unifont hex 路径（默认用 build/font-cache 下的缓存）")
    ap.add_argument("--out", default=os.path.join(REPO, "kernel/src/video/cjk.bin"))
    ap.add_argument("--download", action="store_true", help="先下载 Unifont hex")
    args = ap.parse_args()

    cache = os.path.join(REPO, "build/font-cache")
    if args.hex:
        hex_path = args.hex
    elif args.download:
        hex_path = download_hex(cache)
    else:
        hex_path = os.path.join(cache, f"unifont_all-{UNIFONT_VERSION}.hex")
        if not os.path.exists(hex_path):
            sys.exit(f"[gen] 找不到 {hex_path}，先跑一次 --download")

    glyphs, skipped = parse_hex(hex_path)
    print(f"[gen] Unifont hex: {hex_path}（{len(glyphs)} 个字形, 跳过 {skipped} 行非 8x16/16x16）")

    gb = gb2312_charset()
    repo = repo_charset()
    charset = gb | repo
    print(f"[gen] 字符集: GB2312 {len(gb)} 字 ∪ 仓库非 ASCII {len(repo)} 字 = {len(charset)} 字")

    blob, missing, wide, narrow = build_records(charset, glyphs)
    with open(args.out, "wb") as fh:
        fh.write(blob)

    print(f"[gen] 写出 {args.out}: {len(blob)} 字节 / {len(blob) // RECORD} 字"
          f"（宽 {wide} + 窄 {narrow}）, 记录 {RECORD} 字节")
    if missing:
        preview = "".join(chr(c) for c in missing[:40])
        print(f"[gen] ⚠️ {len(missing)} 字在 Unifont 中无字形（内核画成空心豆腐块）: {preview}")


if __name__ == "__main__":
    main()
