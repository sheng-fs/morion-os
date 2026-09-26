#!/bin/bash
# 模拟镜像全量 FS 回归: 7 个 namespace (FAT32 / MFS / ext2 / 分区盘 / exFAT / 空白盘 / 分区表盘)
# 上跑 app 的 FS-1..FS-26 自测, 由日志判定通过与否。
#
#   bash scripts/fs-regress.sh [日志路径]
#
# 环境变量: MFS_KEEP=1 保留既有 MFS 卷; REGRESS_TIMEOUT_S 放宽等待上限 (无 KVM 的 CI);
#           QEMU_EXTRA 追加 QEMU 参数; BIOS 覆盖 OVMF 路径。
#
# ⏱️ 有 KVM 时整套约 6 分钟: 约 2 万个块请求, IPC 一跳 ≈ 一个时钟 tick, 故有效吞吐
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
# 分区表测试盘同理: FS-26 会建/删分区表, 保留上一轮的 GPT/MBR 会让「在空白盘上建表」
# 这条路径没被走到 (FS-26 自己也会先 wipe 一次, 但重置镜像让起点更干净)。
dd if=/dev/zero of="$OUT_DIR/pt.img" bs=1M count="${PT_MIB:-64}" status=none
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

# 加速: 有 KVM 就用 (-enable-kvm); 没有 (多数 CI runner) 退回 TCG —— 结果一样但要慢
# 好几倍, 故等待上限用 REGRESS_TIMEOUT_S 放宽 (默认 600 s, 按 KVM 下 ~6 分钟定的)。
accel=""
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel="-enable-kvm"
else
  echo "== 无可用 /dev/kvm: QEMU 走 TCG, 会明显变慢 (用 REGRESS_TIMEOUT_S 放宽等待)"
fi
timeout_s=${REGRESS_TIMEOUT_S:-600}

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
  -drive file="$OUT_DIR/pt.img",if=none,id=n7,format=raw -device nvme-ns,drive=n7,bus=nvme0,nsid=7 \
  -display none -monitor none -serial file:"$log" -no-reboot ${accel} ${QEMU_EXTRA:-} \
  >/dev/null 2>&1 &
pid=$!

# 出现结论或失败即提前收工; 否则最多等 timeout_s 秒 (每 5 s 轮询一次)。
for _ in $(seq 1 $((timeout_s / 5))); do
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
echo "== 耗时: $(($(date +%s) - t0)) 秒 (出现结论即停, 最长等 ${timeout_s} 秒)"
echo "== SELFTEST DONE 次数: $done_n"
echo "== FAILED/PANIC 次数: $fail_n"
echo "== 卷表 (block_srv vol: 行) =="
grep -nE '^vol: ' "$log" 2>/dev/null || echo "(无)"
echo "== 额外卷挂载 (mount-dbg) =="
grep -nE 'mount-dbg' "$log" 2>/dev/null || echo "(无)"
echo "== 显式格式化 (mkfs) =="
grep -nE 'mfs: mkfs refused|mfs: mkfs FAILED|mfs: reload state after mkfs' "$log" 2>/dev/null || echo "(无拒绝/无失败)"
echo "== 分区表写入 (part) =="
grep -nE 'part-dbg:|block: part .*refused' "$log" 2>/dev/null || echo "(无)"
# 跨实现校验: FS-26 结束时在 pt.img 上留了一张 GPT, 用宿主工具独立验证它**符合 GPT 规范**
# —— 客户机里复算 CRC32 只能证明这张表「自洽」, 证不了 GUID 字节序 / 头字段这些规范细节。
# sgdisk 缺失时跳过 (判定口径不变; 有它时「不是 no problems found」即判失败)。
host_bad=0
echo "== 分区表宿主校验 (pt.img) =="
if command -v sgdisk >/dev/null 2>&1; then
  sgdisk_out=$(sgdisk -v "$OUT_DIR/pt.img" 2>&1)
  echo "$sgdisk_out" | sed 's/^/  /'
  sgdisk -p "$OUT_DIR/pt.img" 2>&1 | sed 's/^/  /'
  echo "$sgdisk_out" | grep -qi 'no problems found' || host_bad=1
else
  echo "  (无 sgdisk, 跳过宿主校验)"
fi
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" 2>/dev/null || echo "(无)"

[ "${done_n:-0}" -ge 1 ] && [ "${fail_n:-0}" -eq 0 ] && [ "${host_bad:-0}" -eq 0 ]
