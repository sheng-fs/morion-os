#!/bin/bash
# 模拟镜像全量 FS 回归: 6 个 namespace (FAT32 / MFS / ext2 / 分区盘 / exFAT / 空白盘) 上跑
# app 的 FS-1..FS-22 自测, 由日志判定通过与否。
#
#   bash scripts/fs-regress.sh [日志路径]
#
# ⏱️ 整套约 4~4.5 分钟: 约 2 万个块请求, IPC 一跳 ≈ 一个时钟 tick, 故有效吞吐
# 约 100 请求/s。期间日志会长时间「只有 shell 提示符、没有新行」, 这不是卡死。
# 耗时集中在 FS-12 (MFS 目录与长名: 200 项 + 每次 3 遍显式全卷 GC), 单它就 ~170 s。
#
# 退出码: 0 = 自测跑完且无 FAILED/PANIC; 1 = 有失败或没等到结论。
set -u
cd "$(dirname "$0")/.."

OUT_DIR=${OUT_DIR:-build}
log=${1:-/tmp/morion-fs-regress.log}
rm -f "$log"

# MFS 是**持久卷**, 而 FS-5 / FS-10 每轮各留下一个快照 (环形槽位上限 8), 约 4 轮就饱和。
# 快照会永久钉住它当时的可达块, 而 `mfs_gc` 的可达根 = 当前 inode 表 **+ 每个快照**,
# 且对每个根完整重走一遍 —— 于是同一个二进制**越跑越慢** (实测: 空白卷自测 258 s,
# 快照环饱和后 524 s; FS-12 由 171 s 涨到 274 s)。故默认把卷重置成空白
# (mfs_srv 首次挂载会自动格式化), 让每轮耗时可比。想留着上一轮的卷用 MFS_KEEP=1。
# 空白盘 (spare.img) 每轮都重置: FS-22 要验证的正是「格式化一块**没有文件系统**的卷」,
# 保留上一轮格好的卷会让这条路径根本没被走到。
dd if=/dev/zero of="$OUT_DIR/spare.img" bs=1M count="${SPARE_MIB:-16}" status=none
if [ "${MFS_KEEP:-0}" = "1" ]; then
  echo "== 保留既有 MFS 卷: $OUT_DIR/mfs.img (MFS_KEEP=1)"
else
  dd if=/dev/zero of="$OUT_DIR/mfs.img" bs=1M count="${MFS_MIB:-256}" status=none
  echo "== 重置 MFS 卷: $OUT_DIR/mfs.img -> 空白 (首次挂载自动格式化)"
fi

# 记录耗时: 整套自测是一长串 IPC 往返 (每次请求 ≈ 一个时钟 tick), 加自测用例就会变慢,
# 写进输出便于对比「是变慢了还是卡住了」。
t0=$(date +%s)

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}

$QEMU \
  -machine q35 -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
  -cdrom "$OUT_DIR/morion-os.iso" \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file="$OUT_DIR/nvme.img",if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
  -drive file="$OUT_DIR/mfs.img",if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
  -drive file="$OUT_DIR/ext2.img",if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
  -drive file="$OUT_DIR/parts.img",if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
  -drive file="$OUT_DIR/exfat.img",if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
  -drive file="$OUT_DIR/spare.img",if=none,id=n6,format=raw -device nvme-ns,drive=n6,bus=nvme0,nsid=6 \
  -display none -monitor none -serial file:"$log" -no-reboot ${QEMU_EXTRA:-} -enable-kvm &
pid=$!

# 出现结论或失败即提前收工; 否则最多等 10 分钟。
for _ in $(seq 1 120); do
  sleep 5
  if grep -qE 'SELFTEST DONE|KERNEL PANIC|FAILED' "$log" 2>/dev/null; then break; fi
  kill -0 "$pid" 2>/dev/null || break
done
sleep 3
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

done_n=$(grep -c 'SELFTEST DONE' "$log" 2>/dev/null || true)
fail_n=$(grep -cE 'FAILED|PANIC' "$log" 2>/dev/null || true)
echo "== 日志: $log"
echo "== 耗时: $(($(date +%s) - t0)) 秒 (出现结论即停, 最长等 10 分钟)"
echo "== SELFTEST DONE 次数: $done_n"
echo "== FAILED/PANIC 次数: $fail_n"
echo "== 卷表 (block_srv vol: 行) =="
grep -nE '^vol: ' "$log" 2>/dev/null || echo "(无)"
echo "== 额外卷挂载 (mount-dbg) =="
grep -nE 'mount-dbg' "$log" 2>/dev/null || echo "(无)"
echo "== 显式格式化 (mkfs) =="
grep -nE 'mfs: mkfs refused|mfs: mkfs FAILED|mfs: reload state after mkfs' "$log" 2>/dev/null || echo "(无拒绝/无失败)"
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" 2>/dev/null || echo "(无)"

[ "${done_n:-0}" -ge 1 ] && [ "${fail_n:-0}" -eq 0 ]
