#!/bin/bash
# 快速 shell 探测: 启动系统, 经 QEMU monitor 注入若干 shell 命令, 打印串口日志里的相关输出。
#
#   bash scripts/probe-shell.sh "ls -l /mfs" "ln -s /mfs/A.TXT /mfs/L1" "cat /mfs/L1"
#
# 用途: 调文件系统时**不必等整套 3.5~4 分钟的自测**就能看到 shell 级现象 ——
# 自测要跑完全部 FS-1..FS-19 才轮到出问题的用例, 而本脚本约 1 分钟就能复现并给出
# 服务端日志 (自测仍在后台跑, 不干扰这几条命令)。
#
# ⚠️ 按键注入有两个坑 (都踩过):
#   1. **必须慢敲** —— shell 的输入信箱只有 16 条, 快了会丢字甚至把同一条命令提交两次
#      (现象: `ls: cannot open` 的路径少字符、或 `ln` 报"名字已存在"但其实刚建成功)。
#      故每字符 0.5 s、每条命令后等 18 s。
#   2. 字母只有小写, 大写要写 `shift-x`; `+` 是 `shift-equal`, `/` 是 `slash`, 空格 `spc`。
#
# 退出码恒为 0 (这是诊断工具, 不充当门禁)。
set -u
cd "$(dirname "$0")/.."

if [ "$#" -lt 1 ]; then
  echo "用法: $0 \"<shell 命令>\" [\"<shell 命令>\" ...]" >&2
  exit 2
fi

log=${PROBE_LOG:-/tmp/morion-probe.log}
sock=/tmp/morion-probe.sock
rm -f "$log" "$sock"

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}

$QEMU \
  -machine q35 -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
  -cdrom build/morion-os.iso \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file=build/nvme.img,if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
  -drive file=build/mfs.img,if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
  -drive file=build/ext2.img,if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
  -drive file=build/parts.img,if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
  -drive file=build/exfat.img,if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
  -display none -monitor unix:"$sock",server,nowait -serial file:"$log" -no-reboot \
  ${QEMU_EXTRA:-} -enable-kvm &
qpid=$!

python3 - "$sock" "$@" <<'PY'
import socket, sys, time

sock, cmds = sys.argv[1], sys.argv[2:]
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
           "+": "shift-equal", "=": "equal", "'": "apostrophe", ":": "shift-semicolon"}

def key_of(ch):
    if ch in SPECIAL:
        return SPECIAL[ch]
    if ch.isupper():
        return "shift-" + ch.lower()
    return ch

def keys(line):
    for ch in line:
        s.sendall(("sendkey %s\n" % key_of(ch)).encode())
        time.sleep(0.5)
    s.sendall(b"sendkey ret\n")
    time.sleep(18)

time.sleep(50)   # 等启动到 shell 提示符 (挂载诊断已打印)
for c in cmds:
    keys(c)
PY

sleep 3
kill "$qpid" 2>/dev/null
wait "$qpid" 2>/dev/null

echo "== 日志: $log"
echo "== 启动期诊断 (挂载 / 容量) =="
grep -nE 'mfs-dbg|exfat-dbg|mount-dbg|ext2: mount|FAILED|PANIC' "$log" | head -20 || echo "(无)"
echo "== 注入命令后的交互输出 =="
awk '/type .help. for commands/{p=1} p' "$log" | tail -40
