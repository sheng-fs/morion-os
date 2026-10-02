#!/bin/bash
# 快速 shell 探测: 启动系统, 经 QEMU monitor 注入若干 shell 命令, 打印串口日志里的相关输出。
#
#   bash scripts/probe-shell.sh "ls -l /mfs" "ln -s /mfs/A.TXT /mfs/L1" "cat /mfs/L1"
#
# 用途: 调文件系统时**不必等整套 3.5~4 分钟的自测**就能看到 shell 级现象 ——
# 自测要跑完全部 FS-1..FS-22 才轮到出问题的用例, 而本脚本约 1 分钟就能复现并给出
# 服务端日志 (自测仍在后台跑, 不干扰这几条命令)。
#
# ⚠️ 按键注入有两个坑 (都踩过):
#   1. **必须慢敲** —— shell 的输入信箱只有 16 条, 快了会丢字甚至把同一条命令提交两次
#      (现象: `ls: cannot open` 的路径少字符、或 `ln` 报"名字已存在"但其实刚建成功)。
#      故每字符 0.5 s、每条命令后等 18 s。
#   2. 字母只有小写, 大写要写 `shift-x`; `+` 是 `shift-equal`, `/` 是 `slash`, 空格 `spc`。
#
# 退出码恒为 0 (这是诊断工具, 不充当门禁)。
#
# 可选环境变量:
#   PROBE_ISO=...           变体镜像路径 (默认 build/morion-os.iso)
#   PROBE_LOG=...           串口日志路径 (默认 /tmp/morion-probe.log)
#   PROBE_WAIT_PATTERN=...  注入前先等日志里出现该串 (典型: 'SELFTEST DONE'), 上限
#                           PROBE_WAIT_TIMEOUT 秒 (默认 420)。默认**不等待** (固定 50 s) ——
#                           当断言必须在自测**跑完之后**才成立时用它把注入推后: 自测会
#                           造/删文件、拍快照、`mfs.rollback`, 早期注入的现场会被它们搅乱。
#   PROBE_CMD_GAP=...       多条命令之间的间隔秒 (默认 18)。系统忙的时候 18 s 会把后续
#                           命令的按键丢掉 (踩过: 三条只落了第一条), 这时调大它。
set -u
cd "$(dirname "$0")/.."

if [ "$#" -lt 1 ]; then
  echo "用法: $0 \"<shell 命令>\" [\"<shell 命令>\" ...]" >&2
  exit 2
fi

# 空白盘 (build/spare.img) 每轮重置 —— 与 fs-regress.sh 同一约定 (它启动前也 dd 清零)。
# 原因: 自测 FS-22 会把这块盘格式化成 MFS 并把**主卷序号**抬到 max+1; 清不掉的话, 下次
# 启动主卷认领按序号最大者胜 → `/mfs` 变成这块 16 MiB 的 spare, 自测里按「/mfs 是 nsid=2
# 的 256 MiB 卷」写死的几何断言 (FS21/FS23) 就会误报 FAILED (踩过: 探测日志 FS21 FAILED)。
dd if=/dev/zero of=build/spare.img bs=1M count=16 status=none
# 分区表测试盘 (nsid=7): 自测 FS-26 会对它做 part.* 写操作 —— **缺这块盘会让 FS-26 直接
# FAILED 并终止整套自测** (踩过: 日志出现 `block: part wipe FAILED` + `app: FS26 wipe FAILED`
# 之后就不再有 SELFTEST DONE, 于是 PROBE_WAIT_PATTERN 白等满超时)。回归脚本一直挂满
# 7 个 namespace, 这里与之对齐。
dd if=/dev/zero of=build/pt.img bs=1M count=64 status=none

log=${PROBE_LOG:-/tmp/morion-probe.log}
sock=/tmp/morion-probe.sock
rm -f "$log" "$sock"

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}
# 镜像路径可换: 变体产物落在 build/<变体>/ 下 (如 build/install/morion-os.iso),
# 用 PROBE_ISO=... 指过去即可 (磁盘镜像仍用 build/*.img, 变体之间共用测试盘)。
ISO=${PROBE_ISO:-build/morion-os.iso}

$QEMU \
  -machine q35 -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
  -cdrom "$ISO" \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file=build/nvme.img,if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
  -drive file=build/mfs.img,if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
  -drive file=build/ext2.img,if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
  -drive file=build/parts.img,if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
  -drive file=build/exfat.img,if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
  -drive file=build/spare.img,if=none,id=n6,format=raw -device nvme-ns,drive=n6,bus=nvme0,nsid=6 \
  -drive file=build/pt.img,if=none,id=n7,format=raw -device nvme-ns,drive=n7,bus=nvme0,nsid=7 \
  -display none -monitor unix:"$sock",server,nowait -serial file:"$log" -no-reboot \
  ${QEMU_EXTRA:-} -enable-kvm &
qpid=$!

python3 - "$sock" "$log" "$qpid" "${PROBE_WAIT_PATTERN:-}" "${PROBE_WAIT_TIMEOUT:-420}" "${PROBE_CMD_GAP:-18}" "$@" <<'PY'
import os, socket, sys, time

sock, logpath, qpid, wait_pat, wait_to, cmd_gap, cmds = (
    sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7:])
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
    # 条间间隔: 忙的时候 (自测仍在跑) 18 s 会把后续命令的按键丢掉 —— 系统越忙越要放长
    # (踩过: 三条命令只落了第一条)。用 PROBE_CMD_GAP 调。
    time.sleep(float(cmd_gap))

if wait_pat:
    # 先等日志出现约定串 (典型 'SELFTEST DONE') 再注入 —— 超时也照常注入, 让调用方从
    # 日志里看出"没等到", 而不是静默什么都不做。
    t_wait = time.time()
    deadline = t_wait + float(wait_to)
    seen = False
    while time.time() < deadline:
        # QEMU 中途退出 (panic 之后 -no-reboot 会直接退出) 就别再等了 —— 早退比白等满超时
        # 有用得多: 踩过 "自测在 FS-26 失败后整个套件停住, 这里白等 8 分钟什么都没干"。
        if not os.path.exists("/proc/%s" % qpid):
            print("wait: QEMU exited, stop waiting for %r" % wait_pat, flush=True)
            sys.exit(1)
        try:
            with open(logpath, "r", errors="ignore") as fh:
                if wait_pat in fh.read():
                    seen = True
                    break
        except OSError:
            pass
        time.sleep(2)
    print("wait: %r %s (%.0f s)" % (wait_pat, "seen" if seen else "TIMEOUT, injecting anyway",
                                    time.time() - t_wait), flush=True)
    time.sleep(2)
else:
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
