#!/bin/bash
# 模拟镜像全量 FS 回归: 5 个 namespace (FAT32 / MFS / ext2 / 分区盘 / exFAT) 上跑
# app 的 FS-1..FS-17 自测, 由日志判定通过与否。
#
#   bash scripts/fs-regress.sh [日志路径]
#
# ⏱️ 整套约 3.5~4 分钟: 约 2 万个块请求, IPC 一跳 ≈ 一个时钟 tick, 故有效吞吐
# 约 100 请求/s。期间日志会长时间「只有 shell 提示符、没有新行」, 这不是卡死。
#
# 退出码: 0 = 自测跑完且无 FAILED/PANIC; 1 = 有失败或没等到结论。
set -u
cd "$(dirname "$0")/.."

OUT_DIR=${OUT_DIR:-build}
log=${1:-/tmp/morion-fs-regress.log}
rm -f "$log"

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
echo "== SELFTEST DONE 次数: $done_n"
echo "== FAILED/PANIC 次数: $fail_n"
echo "== 额外卷挂载 (mount-dbg) =="
grep -nE 'mount-dbg' "$log" 2>/dev/null || echo "(无)"
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" 2>/dev/null || echo "(无)"

[ "${done_n:-0}" -ge 1 ] && [ "${fail_n:-0}" -eq 0 ]
