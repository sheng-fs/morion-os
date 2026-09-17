#!/bin/bash
# 真机 U 盘**只读**端到端测试 (多卷挂载 + 大簇)。
#
#   bash scripts/usb-ro.sh <分区设备> [<分区设备> ...]
#
# 例: bash scripts/usb-ro.sh /dev/sda1 /dev/sda2 /dev/sdb1
#
# 5 张模拟镜像照常接入 (app 自测只写它们), 真盘分区按顺序作 nsid 6, 7, 8… 接入,
# 卷层解析出文件系统类型后由对应服务自动挂到 `/usb<卷号>`。
#
# 安全: 所有真盘一律 `format=raw,readonly=on` —— QEMU 层拒绝一切写入, 物理盘不可改;
# 脚本另在前后各算一次盘头 sha256 作交叉验证。前提是当前用户对该块设备有**读**权限:
#   sudo chgrp $(id -gn) /dev/sda1 /dev/sda2 && sudo chmod 440 /dev/sda1 /dev/sda2
# (临时授权, 拔插设备即恢复。注意重插后盘符会变, 用 lsblk 确认名字。)
#
# 退出码: 0 = 自测跑完且无 FAILED/PANIC 且盘头未变。
set -u
cd "$(dirname "$0")/.."

if [ "$#" -lt 1 ]; then
  echo "用法: $0 <分区设备> [<分区设备> ...]   (如 /dev/sda1 /dev/sda2)" >&2
  exit 2
fi

OUT_DIR=${OUT_DIR:-build}
log=/tmp/morion-usb-ro.log
sock=/tmp/morion-usb-ro.sock
head_before=/tmp/morion-usb-head-before.txt
head_after=/tmp/morion-usb-head-after.txt
rm -f "$log" "$sock" "$head_before" "$head_after"

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}

for d in "$@"; do
  if ! head -c 512 "$d" > /dev/null 2>&1; then
    echo "无法读取 $d —— 当前用户没有该块设备的读权限:" >&2
    echo "  sudo chgrp \$(id -gn) $* && sudo chmod 440 $*" >&2
    exit 2
  fi
done

# 真盘作 nsid 6,7,8… 接入 (readonly)。
extra=()
nsid=6
for d in "$@"; do
  extra+=(-drive "file=$d,if=none,id=usb$nsid,format=raw,readonly=on"
          -device "nvme-ns,drive=usb$nsid,bus=nvme0,nsid=$nsid")
  nsid=$((nsid + 1))
done
NDEV=$#

echo "== 测试前盘头 sha256 (前 8 MiB) =="
for d in "$@"; do printf "%s  " "$d"; dd if="$d" bs=1M count=8 status=none | sha256sum; done | tee "$head_before"

$QEMU \
  -machine q35 -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
  -cdrom "$OUT_DIR/morion-os.iso" \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file="$OUT_DIR/nvme.img",if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
  -drive file="$OUT_DIR/mfs.img",if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
  -drive file="$OUT_DIR/ext2.img",if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
  -drive file="$OUT_DIR/parts.img",if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
  -drive file="$OUT_DIR/exfat.img",if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
  "${extra[@]}" \
  -display none -monitor unix:"$sock",server,nowait -serial file:"$log" -no-reboot -enable-kvm &
qpid=$!

# 经 QEMU monitor 注入按键, 在 shell 里跑几条只读命令。
# ⚠️ sendkey 对字母只出小写, 大写要写 `shift-x`; `+` 是 `shift-equal`。
python3 - "$sock" "$NDEV" <<'PY'
import socket, sys, time

sock, ndev = sys.argv[1], int(sys.argv[2])
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(60):
    try:
        s.connect(sock)
        break
    except OSError:
        time.sleep(1)
else:
    print("monitor connect FAILED")
    sys.exit(1)

SPECIAL = {"/": "slash", " ": "spc", ".": "dot", "-": "minus", "_": "shift-minus",
           "+": "shift-equal", "=": "equal"}

def key_of(ch):
    if ch in SPECIAL:
        return SPECIAL[ch]
    if ch.isupper():
        return "shift-" + ch.lower()
    return ch

def keys(line):
    for ch in line:
        s.sendall(("sendkey %s\n" % key_of(ch)).encode())
        time.sleep(0.35)   # 慢敲: 自测同时在跑, shell 的输入信箱只有 16 条
    s.sendall(b"sendkey ret\n")
    time.sleep(14)

time.sleep(40)                     # 等启动到 shell 提示符
for i in range(ndev):
    keys("ls /usb%d" % (6 + i))    # 真盘每个分区的根目录
keys("ls /")                       # 默认挂载树必须不受影响
PY

# 自测整套约 3.5~4 分钟。
for _ in $(seq 1 80); do
  sleep 5
  if grep -qE 'SELFTEST DONE|KERNEL PANIC|FAILED' "$log" 2>/dev/null; then break; fi
  kill -0 "$qpid" 2>/dev/null || break
done
sleep 3
kill "$qpid" 2>/dev/null
wait "$qpid" 2>/dev/null

echo "== 测试后盘头 sha256 =="
for d in "$@"; do printf "%s  " "$d"; dd if="$d" bs=1M count=8 status=none | sha256sum; done | tee "$head_after"

unchanged=1
if diff -q "$head_before" "$head_after" >/dev/null 2>&1; then
  echo "== 真盘盘头均未变 ✅"
else
  echo "== 有盘被改 ❌"
  unchanged=0
fi

done_n=$(grep -c 'SELFTEST DONE' "$log" 2>/dev/null || true)
fail_n=$(grep -cE 'FAILED|PANIC' "$log" 2>/dev/null || true)
echo "== 日志: $log"
echo "== SELFTEST DONE 次数: $done_n"
echo "== FAILED/PANIC 次数: $fail_n"
echo "== 卷层 / 挂载诊断 (mount-dbg 即 /usb<卷号> 挂载结果) =="
grep -nE 'mount-dbg|unsupported geometry|mount FAILED|refuse to format|KERNEL PANIC' "$log" 2>/dev/null || echo "(无)"
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" 2>/dev/null || echo "(无)"

[ "${done_n:-0}" -ge 1 ] && [ "${fail_n:-0}" -eq 0 ] && [ "$unchanged" -eq 1 ]
