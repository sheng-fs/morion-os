#!/usr/bin/env bash
# MorionFS `mfs.fsck --repair` 的**泄漏回收**用例 (文件系统流 01 的覆盖缺口补测)
#
# 为什么不能做成 app 自测: 「已分配但不可达」的 inode 只出现在**两次 COW 提交之间掉电**的
# 窗口里 —— unlink 是「mfs_dir_delete(摘目录项) 提交成功, mfs_free_ino(释放 inode) 还没提交」;
# creat 是「对象块 + inode 槽已提交, mfs_dir_insert(插目录项) 还没提交」。这两步各自独立提交,
# 所以窗口真实存在, 但**用户态 API 造不出来** (creat 一定跟着插目录项, unlink 一定跟着释放),
# app 自测跑在客户机里也没法"在两次提交之间拔电"。故本脚本按 FS-30 的办法做: 宿主侧直接改
# 镜像, 人为把现场改成那个形态, 再让客户机去报案 (mfs.fsck) 与回收 (mfs.fsck --repair)。
#
# 步骤 (**三轮启动**, 每轮都先等自测跑完再注入 —— 自测会造/删文件、拍快照、回滚, 现场必须静止;
#        每条命令单独一轮也是为了避开 probe 按键注入在系统忙时丢字):
#   轮 1) 重置 build/mfs.img 为空白 (首次挂载自动格式化) -> 注入 `touch /mfs/LEAK.TXT`
#   轮 2) 宿主侧把该目录项改成**空槽** (name_len=0 且 ino=0, 并重算块头 CRC), 然后:
#         注入 `mfs.fsck`            -> 应报 1 个泄漏
#         注入 `mfs.fsck --repair`   -> 应回收 1 个
#   轮 3) 注入 `mfs.fsck`            -> 应报 0 (**重启之后**再看, 顺便证明修复落了盘)
#
# 用法: bash scripts/fsck-leak.sh
# 前提: build/morion-os.iso (先 make iso) / python3 / qemu-system-x86_64 / scripts/probe-shell.sh
# 耗时: 三轮各约 5~6 分钟 (都在等自测结束), 合计 ~17 分钟
#
# ⚠️ 预期内的现象, 别当失败:
#   1) 第 2/3 轮的卷上带着上一轮自测的遗留 (如 /mfs/D) 与**故意**制造的 1 个泄漏, 自测里会有
#      若干条照实打 `FAILED` (实测先停在 `app: FS5 mkdir /mfs/D FAILED`; 泄漏本身让 FS-31 报出
#      非 0 泄漏数) —— 本用例的判定**只看**日志里的 `mfs.fsck:` 行。
#   2) 脚本会重置 build/mfs.img (与 fs-regress.sh 同一约定: 测试镜像归测试), 结束时会**还原**
#      原镜像 (备份留在 $WORK/mfs.img.bak)。
#
# 退出码: 0 = 三条断言全对; 1 = 有断言不符或前置不满足。
set -u
cd "$(dirname "$0")/.."

OUT_DIR=${OUT_DIR:-build}
ISO=${PROBE_ISO:-$OUT_DIR/morion-os.iso}
MFS_IMG=$OUT_DIR/mfs.img
# 必须与 Makefile 的 MFS_MIB 一致 (256): 自测 FS-21/FS-23 按「/mfs 是 nsid=2 的 256 MiB 卷」
# 写死了几何断言, 换成别的容量会误报 FAILED (probe-shell.sh 头部也记过同一个坑)。
MFS_MIB=${MFS_MIB:-256}
WORK=${WORK:-/tmp/mfs-leak}
L1=$WORK/boot1.log
L2=$WORK/boot2.log
L3=$WORK/boot3.log
PAT='SELFTEST DONE'
WAIT=${PROBE_WAIT_TIMEOUT:-480}

say() { echo "== $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }

[ -f "$ISO" ] || die "缺少 ISO: $ISO (先 make iso)"
command -v python3 >/dev/null 2>&1 || die "需要 python3 (改镜像用)"
[ -f scripts/probe-shell.sh ] || die "找不到 scripts/probe-shell.sh"
# 起跑前先查残留 QEMU: 它会**锁住 build/*.img**, 后续 QEMU 一律 "Failed to get write lock"
# 秒退 (本用例三轮启动, 中途 Ctrl-C 很容易留下孤立的 QEMU —— 踩过两次)。
if pgrep -f 'qemu-system-x86_64' >/dev/null 2>&1; then
  pgrep -af 'qemu-system-x86_64' | sed 's/^/  残留: /'
  die "有残留 QEMU 占着镜像锁, 先杀掉它们再跑 (kill <pid>)"
fi
mkdir -p "$WORK"

if [ -f "$MFS_IMG" ]; then cp "$MFS_IMG" "$WORK/mfs.img.bak"; fi
restore() { [ -f "$WORK/mfs.img.bak" ] && cp "$WORK/mfs.img.bak" "$MFS_IMG"; }
trap restore EXIT

# 统一的启动+注入封装: 先等自测跑完 (PAT), 再敲命令。
boot() {   # boot <日志> <间隔秒> <命令...>
  local log=$1 gap=$2; shift 2
  PROBE_LOG="$log" PROBE_WAIT_PATTERN="$PAT" PROBE_WAIT_TIMEOUT="$WAIT" PROBE_CMD_GAP="$gap" \
    bash scripts/probe-shell.sh "$@" >/dev/null 2>&1 || true
}

say "1/3 重置 MFS 卷 (${MFS_MIB} MiB) 并建一个文件"
dd if=/dev/zero of="$MFS_IMG" bs=1M count="$MFS_MIB" status=none
boot "$L1" 18 "touch /mfs/LEAK.TXT"
grep -aq "$PAT" "$L1" 2>/dev/null || die "第 1 轮没等到自测完成 (日志: $L1)"

say "2/3 宿主侧把目录项改成空槽 (制造 1 个泄漏 inode), 再报案 + 回收"
python3 - "$MFS_IMG" <<'PY' || die "改镜像失败 (见上面的输出)"
import sys
import zlib

path = sys.argv[1]
BLK, HDR, ENT_HDR = 4096, 8, 8     # 块 = 4 KiB; 块头 8 B; 目录项头 8 B (+8 起是名字)
MAGIC_DIR, MAGIC_DIDX = 0x4D46_4449, 0x4D46_5849   # "MFDI" / "MFXI"
NAME = b"LEAK.TXT"

data = bytearray(open(path, "rb").read())
hit = 0
i = data.find(NAME)
while i != -1:
    e = i - ENT_HDR                                  # 条目起点 = 名字偏移 - 8
    if e >= 0 and e % 4 == 0:
        blk = (e // BLK) * BLK
        magic = int.from_bytes(data[blk:blk + 4], "little")
        rec = int.from_bytes(data[e + 6:e + 8], "little")
        # 三重校验, 免得改到"文件内容里恰好出现这串名字"之外的东西
        if (magic in (MAGIC_DIR, MAGIC_DIDX) and data[e + 5] == len(NAME)
                and data[e + 4] in (1, 2) and 12 <= rec <= BLK):
            data[e:e + 4] = b"\x00\x00\x00\x00"      # ino = 0
            data[e + 5] = 0                          # name_len = 0 -> 条目不可见
            # ⚠️ 块头 CRC 覆盖 payload(偏移 8..块尾), 改了 payload 必须重算 —— 否则 mfs_ok
            # 校验不过, fsck 的目录遍历会当场放弃 (踩过: `mfs: fsck walk FAILED`)。
            # 算法与 common.rs 的 mfs_crc32 一致: CRC-32/IEEE(反射, 0xEDB88320), 即 zlib.crc32。
            crc = zlib.crc32(bytes(data[blk + HDR:blk + BLK])) & 0xFFFF_FFFF
            data[blk + 4:blk + 8] = crc.to_bytes(4, "little")
            hit += 1
            print("patched entry @0x%x (block @0x%x, rec_len=%d, 重算块 CRC=0x%08x)"
                  % (e, blk, rec, crc))
    i = data.find(NAME, i + 1)
if hit == 0:
    print("FAIL: 镜像里找不到可识别的 LEAK.TXT 目录项")
    sys.exit(1)
open(path, "wb").write(data)
print("patched %d entry(ies): 现场 = 目录项已摘除但 inode 槽仍占用 (1 个泄漏 inode)" % hit)
PY
boot "$L2" 45 "mfs.fsck" "mfs.fsck --repair"

say "3/3 重启后复查 (顺便证明修复落了盘)"
boot "$L3" 18 "mfs.fsck"

seq2=$(grep -a -oE 'mfs\.fsck: (repaired )?[0-9]+ leaked inode\(s\)' "$L2" 2>/dev/null || true)
seq3=$(grep -a -oE 'mfs\.fsck: (repaired )?[0-9]+ leaked inode\(s\)' "$L3" 2>/dev/null || true)
l1=$(printf '%s\n' "$seq2" | sed -n 1p)
l2=$(printf '%s\n' "$seq2" | sed -n 2p)
l3=$(printf '%s\n' "$seq3" | sed -n 1p)
echo "  报案 (mfs.fsck)          : ${l1:-<缺>}"
echo "  回收 (mfs.fsck --repair) : ${l2:-<缺>}"
echo "  复查 (重启后 mfs.fsck)   : ${l3:-<缺>}"

rc=0
[ "$l1" = "mfs.fsck: 1 leaked inode(s)" ]          || { echo "FAIL: 报案应为 1 个泄漏"; rc=1; }
[ "$l2" = "mfs.fsck: repaired 1 leaked inode(s)" ] || { echo "FAIL: 回收应为 repaired 1"; rc=1; }
[ "$l3" = "mfs.fsck: 0 leaked inode(s)" ]          || { echo "FAIL: 复查应为 0"; rc=1; }

echo
if [ "$rc" -eq 0 ]; then
  echo "== PASS: 报案 1 -> --repair 回收 1 -> 重启复查 0 (泄漏回收路径已被覆盖)"
else
  echo "== FAIL: 见上面的 FAIL 行 (日志: $L1 / $L2 / $L3)"
fi
echo "  注: 第 2/3 轮日志里的自测 FAILED (FS5 mkdir /mfs/D 已存在、FS31 报非 0 泄漏) 属**预期**。"
exit $rc
