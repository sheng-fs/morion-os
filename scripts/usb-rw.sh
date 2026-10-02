#!/bin/bash
# 真机 U 盘**读写**端到端测试 (QEMU 直通真盘, **可写**)。
#
#   bash scripts/usb-rw.sh <整盘设备> [--yes]
#
# 例: bash scripts/usb-rw.sh /dev/sda --yes
#
# ⚠️ 本脚本会**擦除**目标盘 —— 流程即「整盘擦除重建」:
#      宿主清零盘头 (抹掉旧文件系统签名, 见下方「预清零」注释) ->
#      part.wipe 清分区表 -> part.create 重建 MBR 分区 -> mkfs.mfs 建 MorionFS
#      -> 写文件 -> 二次启动读回。只在「无重要数据」的盘上运行。
#    默认需交互输入 yes; `--yes` 跳过确认 (供自动化)。
#
# 与 usb-ro.sh 的判据正相反: 那边 `readonly=on` + 盘头 sha256 **必须不变**;
# 这边盘**必须变**, 故改为两条更强的判据:
#   (1) 宿主侧直读分区起始: 必须出现 MFS8 magic (MorionOS 真的把超级块写下去了);
#   (2) 第二次启动 (readonly): `/usb<卷号>` 必须列出第一次写的文件 (持久化)。
#
# 前提 (当前用户对块设备有读写权限; 拔插设备后权限恢复, 需重新授权):
#   sudo chgrp $(id -gn) /dev/sda && sudo chmod 660 /dev/sda
#
# 退出码: 0 = 擦除重建 / 格式化 / 写入 / 重启读回 全部成功。
#
# 实测 (2026-10-02, thinkplus 238.5G /dev/sda, 退出码 0): 两条判据全过 —— 宿主侧读到 MFS8
# magic; 只读重启后 /usb6 列出 HELLO.TXT / DIR1 / BIG.BIN(size=1048576); 卷容量被
# MFS_MAX_BLOCKS clamp 到 130304 MiB (127.25 GiB), 而盘容量 244198 MiB; 首次启动 FAILED/PANIC=0。
# 已知边界: 盘上残留的文件系统签名 (如出厂 exfat) 会让「新分区」仍被卷层探测成它, 而 mfs_srv
# 拒绝格式化非空白卷 —— 故本脚本在擦除前先清零盘头 (见下方「预清零」注释)。
set -u
cd "$(dirname "$0")/.."

DEV=""
ASSUME_YES=0
for a in "$@"; do
  case "$a" in
    --yes) ASSUME_YES=1 ;;
    *) DEV="$a" ;;
  esac
done
if [ -z "$DEV" ]; then
  echo "用法: $0 <整盘设备> [--yes]   (如 /dev/sda)" >&2
  exit 2
fi
if [ ! -b "$DEV" ] && [ ! -f "$DEV" ]; then
  echo "不是块设备或镜像文件: $DEV" >&2
  exit 2
fi
if [ ! -r "$DEV" ] || [ ! -w "$DEV" ]; then
  echo "无法读写 $DEV —— 当前用户缺权限:" >&2
  echo "  sudo chgrp $(id -gn) $DEV && sudo chmod 660 $DEV" >&2
  exit 2
fi

OUT_DIR=${OUT_DIR:-build}
log=/tmp/morion-usb-rw.log
sock=/tmp/morion-usb-rw.sock
log2=/tmp/morion-usb-rw-reboot.log
sock2=/tmp/morion-usb-rw-reboot.sock
rm -f "$log" "$sock" "$log2" "$sock2"

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}

size_bytes=$(blockdev --getsize64 "$DEV" 2>/dev/null || echo 0)
[ "$size_bytes" -eq 0 ] 2>/dev/null && size_bytes=$(stat -c %s "$DEV" 2>/dev/null || echo 0)   # 演练用的普通文件
echo "== 目标盘: $DEV ($((size_bytes / 1024 / 1024)) MiB, TRAN=$(lsblk -ndo TRAN "$DEV" 2>/dev/null))"
echo "== 测试前盘头 sha256 (前 8 MiB, 预期测试后**变化**) =="
dd if="$DEV" bs=1M count=8 status=none 2>/dev/null | sha256sum

if [ "$ASSUME_YES" -ne 1 ]; then
  echo
  echo "⚠️  即将擦除 $DEV 上的全部分区与数据 (part.wipe + part.create + mkfs.mfs)。"
  printf "确认继续请输入 yes: "
  read -r ans
  [ "$ans" = "yes" ] || { echo "已取消。"; exit 1; }
fi

# ---------------------------------------------------------------------------
# 宿主侧预清零 (盘头): 真盘出厂常带文件系统 (本机这块 thinkplus 是 exfat)。
# part.wipe 只清**分区表**, 不动数据 —— 残留的 exFAT/FAT VBR 会让「新建的分区」仍被
# 卷层探测成 exfat, 而 mfs_srv 的护栏会(正确地)拒绝格式化非空白卷, 于是 mkfs.mfs
# 永远走不通 (实测: 新分区 lba=2048 与旧 exfat 分区完全重叠 -> kind=exfat)。
# 先把盘头清零, 让新分区探测为 unknown —— 这才是「整盘擦除重建」的起点。
# 只清前 256 MiB: 够覆盖 MBR/GPT 与各分区起始的 VBR/FAT 头, 不必白擦整块 238 GiB。
# ---------------------------------------------------------------------------
echo
echo "== 宿主侧预清零盘头 (前 256 MiB): 抹掉旧文件系统签名 =="
dd if=/dev/zero of="$DEV" bs=1M count=256 conv=fsync status=none
# 校验用「与全零区间的 sha256 比对」—— 别用 od 输出判空: od 会把重复行折叠成 `*`,
# 全零输入反而得到非空文本 (踩过, 误报"清零失败")。
zeros_hash=$(head -c 1048576 /dev/zero | sha256sum | cut -d' ' -f1)
head_hash=$(dd if="$DEV" bs=1M count=1 status=none | sha256sum | cut -d' ' -f1)
if [ "$head_hash" = "$zeros_hash" ]; then
  echo "  ✓ 盘头已清零 (前 1 MiB 全 0)"
else
  echo "  ✗ 盘头清零失败 —— 前 1 MiB 仍有非零字节" >&2
  exit 2
fi

# 真盘作 nsid 6 (与 usb-ro.sh 一致); 模拟镜像照常接入。
# 第一次启动**可写** (去掉 readonly=on); 第二次启动只读。
#
# ⚠️ 本函数**不能在 `$( )` 里调用**: 命令替换的 stdout 是管道, 后台 QEMU 会继承它的
# 写端常开, bash 读不到 EOF 就永久挂起 (踩过: 表现为"注入块根本没执行"的假时机问题)。
# 故由函数把 pid 写进全局 `qemu_pid`, stdout/stderr 另存一份 qemu 日志。
qemu_pid=""
qemu_common() {
  local logf=$1 sockf=$2 ro=$3
  local roopt=""
  [ "$ro" = "ro" ] && roopt=",readonly=on"
  $QEMU \
    -machine q35 -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
    -cdrom "$OUT_DIR/morion-os.iso" \
    -device nvme,serial=MORION,id=nvme0 \
    -drive file="$OUT_DIR/nvme.img",if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
    -drive file="$OUT_DIR/mfs.img",if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
    -drive file="$OUT_DIR/ext2.img",if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
    -drive file="$OUT_DIR/parts.img",if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
    -drive file="$OUT_DIR/exfat.img",if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
    -drive "file=$DEV,if=none,id=usb6,format=raw$roopt" -device nvme-ns,drive=usb6,bus=nvme0,nsid=6 \
    -display none -monitor unix:"$sockf",server,nowait -serial file:"$logf" -no-reboot -enable-kvm \
    > "${logf%.log}.qemu.log" 2>&1 &
  qemu_pid=$!
}

# ---------------------------------------------------------------------------
# 第一次启动 (可写): 擦除 -> 分区 -> mkfs.mfs -> 写文件
# ---------------------------------------------------------------------------
echo
echo "== [1/2] 第一次启动 (真盘可写): 擦除重建 + 格式化 + 写文件 =="
qemu_common "$log" "$sock" rw
qpid=$qemu_pid

python3 -u - "$sock" "$log" <<'PY'
import re, socket, sys, time

sock, logpath = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(90):
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

def keys(line, wait):
    for ch in line:
        s.sendall(("sendkey %s\n" % key_of(ch)).encode())
        time.sleep(0.5)    # 慢敲: shell 输入信箱只有 16 条, 自测同时在跑
    s.sendall(b"sendkey ret\n")
    time.sleep(wait)

def wait_for_shell(logpath, timeout=220):
    """等 shell 提示符出现 —— gfx_srv 接管后启动链变长, 固定 sleep 会注入过早(字符全丢)。"""
    for _ in range(timeout):
        try:
            with open(logpath, errors="ignore") as f:
                if "type 'help' for commands" in f.read():
                    return True
        except OSError:
            pass
        time.sleep(1)
    return False

def vol_for_nsid(nsid):
    """从日志里找该 nsid 上**最后一个 lba>0 的分区卷** (整盘卷 lba=0, 不算)。"""
    best = None
    try:
        with open(logpath, errors="ignore") as f:
            for ln in f:
                m = re.match(r"vol: (\d+) nsid=(\d+) lba=(\d+) sectors=(\d+) kind=(\w+)", ln)
                if not m:
                    continue
                vid, ns, lba, sec, kind = (int(m.group(1)), int(m.group(2)),
                                           int(m.group(3)), int(m.group(4)), m.group(5))
                if ns == nsid and lba > 0:
                    best = (vid, lba, sec, kind)
    except OSError:
        pass
    return best

print("== 等待 shell 就绪 (gfx 接管后启动链较长; 自测同时在跑) ==")
if not wait_for_shell(logpath):
    print("等待 shell 超时 (见 %s)" % logpath)
    sys.exit(1)
time.sleep(3)

print("== 阶段 1: 擦除分区表 + 重建 MBR 分区 ==")
keys("part.wipe 6", 18)
keys("part.create 6 0 mbr", 20)
keys("part.reload", 18)          # 保证日志落下最新卷表

v = vol_for_nsid(6)
if not v:
    print("无法从日志解析 nsid=6 的分区卷 (见 %s)" % logpath)
    sys.exit(1)
vid, lba, sec, kind = v
print("== 新分区卷: vol=%d lba=%d sectors=%d kind=%s" % (vid, lba, sec, kind))
if kind not in ("unknown", "mfs"):
    print("新分区卷类型异常: %s" % kind)
    sys.exit(1)

print("== 阶段 2: mkfs.mfs + 写文件 ==")
keys("mkfs.mfs %d" % vid, 25)     # 238.5G 盘: 格式化会按 127.25 GiB 上限 clamp
keys("ls /usb%d" % vid, 10)       # 刚格式化: 应为空目录
keys("touch /usb%d/HELLO.TXT" % vid, 10)
keys("mkdir /usb%d/DIR1" % vid, 10)
keys("touch /usb%d/BIG.BIN" % vid, 10)
keys("truncate /usb%d/BIG.BIN 1048576" % vid, 12)   # 1 MiB 稀疏文件: 重启后看 size 是否如实
keys("ls -l /usb%d" % vid, 12)
keys("df", 12)

# mkfs.mfs 会把目标卷升为主卷 (serial = 现有最大 + 1), 于是**下次启动**真盘会认领 `/mfs`。
# 读回阶段更希望它在 `/usb<卷号>` 露面 (而且只读启动下 /mfs 会让自测的写用例报错),
# 故这里用 `mfs.primary` 把主卷还给基础镜像 (nsid=2 的整盘卷)。
#
# 序号口径 (实测): 两个命令算 "现有最大 + 1" 时只看**卷表里 kind=mfs** 的卷, 而真盘分区是
# 本次 `part.create` 新造的, 在冻结的卷表里仍是 unknown —— 于是二者都拿到 1, 打成平手;
# 认领端 (`mfs_vol_claim`) 平手时**先扫描者胜** (卷表按 nsid 升序), 基础镜像在 nsid=2、
# 真盘在 nsid=6, 故仍是基础镜像当 /mfs、真盘挂 /usb<卷号>。信封条件是脚本自己钉死的
# (真盘恒为 nsid=6), 不是碰运气。
base = None
try:
    with open(logpath, errors="ignore") as f:
        for ln in f:
            m = re.match(r"vol: (\d+) nsid=2 lba=0 sectors=(\d+) kind=(\w+)", ln)
            if m:
                base = int(m.group(1))
except OSError:
    pass
if base is not None:
    print("== 把主卷还回基础镜像 (vol=%d): 重启后真盘挂 /usb%d ==" % (base, vid))
    keys("mfs.primary %d" % base, 15)
    keys("ls /usb%d" % vid, 12)
else:
    print("警告: 没找到 nsid=2 的整盘 MFS 卷, 跳过主卷归还 (重启后真盘会成为 /mfs)")
print("== 注入完成 ==")
PY

# 注入已跑完 (python 是同步的); 再给它一点时间把写入落到盘上 —— 以「ls 里出现
# HELLO.TXT」为证据停, 不拿 FAILED 字样当判据 (shell 的 `screen console mirror FAILED`
# 是无害提示, 早就在日志里了, 拿它当停机会把最后一条命令截断)。
for _ in $(seq 1 60); do
  sleep 2
  grep -q "HELLO.TXT" "$log" 2>/dev/null && break
  kill -0 "$qpid" 2>/dev/null || break
done
sleep 3
kill "$qpid" 2>/dev/null
wait "$qpid" 2>/dev/null

# ---------------------------------------------------------------------------
# 宿主侧验证: 分区起始必须出现 MFS8 超级块 magic
# ---------------------------------------------------------------------------
echo
echo "== 宿主验证: 读回真盘, 找 MFS8 超级块 magic (0x4D463338 = \"MFS8\") =="
mfs_magic=0
# 扫前 4 MiB: 超级块在分区起始 (块 0) 与块 1, 也兼容 lba 对齐差异。
# ⚠️ 字节序: 盘上 magic 是常量 0x4D46_5338 的**小端**落盘, 即 "38 53 46 4d"
# (按 ASCII 读成 "8SFM")。按 4d 46 53 38 找会误报「没写下去」。
if dd if="$DEV" bs=1M count=4 status=none 2>/dev/null | od -A d -t x1 \
     | grep -qE "38 53 46 4d|4d 46 53 38"; then
  mfs_magic=1
  echo "  ✓ 找到 MFS8 超级块 magic —— MorionOS 确实把文件系统写到了真盘"
else
  echo "  ✗ 未找到 MFS8 magic —— 真盘上没落下 MorionFS"
fi

# ---------------------------------------------------------------------------
# 宿主侧解码超级块: 直接给出卷容量 (块 0 的 payload 从块内 +8 起:
#   +8 version / +12 block_size / +16 total_blocks / +20 ino_count / +28 snap_count)
# 用途: 量化 238.5G 盘被 MFS_MAX_BLOCKS (1018 位图块 × 32768 块/页) 上限 clamp 的行为 ——
# 期望看到 总块数=33357824 → 130304 MiB (127.25 GiB), 小于盘本身的容量。
# ---------------------------------------------------------------------------
echo
echo "== 宿主侧解码 MFS 超级块 (分区起点 +1 MiB, part.create 的对齐起点) =="
sbwords=$(dd if="$DEV" bs=1 skip=$((1048576 + 8)) count=16 status=none 2>/dev/null | od -A n -t u4)
sb_ver=$(echo "$sbwords" | awk '{print $1}')
sb_bsize=$(echo "$sbwords" | awk '{print $2}')
sb_total=$(echo "$sbwords" | awk '{print $3}')
sb_inos=$(echo "$sbwords" | awk '{print $4}')
if [ -n "$sb_total" ] && [ "$sb_total" -gt 0 ] 2>/dev/null && [ "$sb_ver" = "8" ]; then
  sb_mib=$(awk -v t="$sb_total" -v b="$sb_bsize" 'BEGIN{printf "%.0f", t*b/1048576}')
  sb_gib=$(awk -v m="$sb_mib" 'BEGIN{printf "%.2f", m/1024}')
  echo "  块大小=$sb_bsize  总块数=$sb_total  inode 数=$sb_inos"
  echo "  卷容量 = ${sb_mib} MiB (${sb_gib} GiB); 盘容量 = $((size_bytes / 1024 / 1024)) MiB"
  if [ "$sb_mib" -lt "$((size_bytes / 1024 / 1024))" ]; then
    echo "  ✓ 卷容量小于盘容量 —— MFS_MAX_BLOCKS 上限 clamp 生效 (预期 130304 MiB / 127.25 GiB)"
  fi
else
  echo "  (超级块解码失败 —— 未格式化? 读回: $sbwords)"
fi

# ---------------------------------------------------------------------------
# 第二次启动 (只读): 验证重启后文件仍在
# ---------------------------------------------------------------------------
echo
echo "== [2/2] 第二次启动 (只读): 重启后读回文件 =="
qemu_common "$log2" "$sock2" ro
qpid=$qemu_pid

python3 -u - "$sock2" "$log2" <<'PY'
import re, socket, sys, time

sock, logpath = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(90):
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

def keys(line, wait):
    for ch in line:
        s.sendall(("sendkey %s\n" % key_of(ch)).encode())
        time.sleep(0.5)
    s.sendall(b"sendkey ret\n")
    time.sleep(wait)

def wait_for_shell(logpath, timeout=220):
    for _ in range(timeout):
        try:
            with open(logpath, errors="ignore") as f:
                if "type 'help' for commands" in f.read():
                    return True
        except OSError:
            pass
        time.sleep(1)
    return False

def vol_for_nsid(nsid):
    best = None
    try:
        with open(logpath, errors="ignore") as f:
            for ln in f:
                m = re.match(r"vol: (\d+) nsid=(\d+) lba=(\d+) sectors=(\d+) kind=(\w+)", ln)
                if not m:
                    continue
                vid, ns, lba, sec, kind = (int(m.group(1)), int(m.group(2)),
                                           int(m.group(3)), int(m.group(4)), m.group(5))
                if ns == nsid and lba > 0 and kind == "mfs":
                    best = (vid, lba, sec, kind)
    except OSError:
        pass
    return best

if not wait_for_shell(logpath):
    print("等待 shell 超时 (见 %s)" % logpath)
    sys.exit(1)
time.sleep(8)            # 提示符之后挂载诊断可能还在收尾, 留点余量
v = vol_for_nsid(6)
if not v:
    print("重启后 nsid=6 上没有 mfs 分区卷")
    sys.exit(1)
vid = v[0]
print("== 重启后真盘分区卷: vol=%d sectors=%d" % (vid, v[2]))
keys("ls /usb%d" % vid, 18)
keys("ls -l /usb%d" % vid, 14)   # 再敲一遍: 万一第一次撞上挂载收尾, 第二次必中 (且带大小)
print("== 注入完成 ==")
PY

for _ in $(seq 1 60); do
  sleep 2
  grep -q "HELLO.TXT" "$log2" 2>/dev/null && break   # 读回证据即停
  kill -0 "$qpid" 2>/dev/null || break
done
sleep 3
kill "$qpid" 2>/dev/null
wait "$qpid" 2>/dev/null

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
echo
echo "== 测试后盘头 sha256 (与测试前对比: 应**不同**) =="
dd if="$DEV" bs=1M count=8 status=none 2>/dev/null | sha256sum

reboot_ok=0
# 读回证据: 文件在 (HELLO.TXT) **且** 稀疏文件的 size 如实 (1048576) —— 后者顺带证明
# 元数据/大小字段跨重启是真的持久化, 不是"目录项在但内容是空的"。
if grep -q "HELLO.TXT" "$log2" 2>/dev/null && grep -q "1048576" "$log2" 2>/dev/null; then
  reboot_ok=1
fi

done_n=$(grep -c 'SELFTEST DONE' "$log" 2>/dev/null || true)
fail_n=$(grep -cE 'FAILED|PANIC' "$log" 2>/dev/null || true)
fail2_n=$(grep -cE 'FAILED|PANIC' "$log2" 2>/dev/null || true)

echo "== 第一次启动: SELFTEST DONE=$done_n, FAILED/PANIC=$fail_n  (日志 $log)"
echo "== 第二次启动: FAILED/PANIC=$fail2_n  (日志 $log2)"
echo "== 第一次启动的写入诊断 =="
grep -nE 'part\.|mkfs\.|HELLO|df:|mfs-dbg' "$log" 2>/dev/null | tail -25 || echo "(无)"
echo "== 第二次启动的读回诊断 =="
grep -nE 'HELLO|mount-dbg|mfs-dbg' "$log2" 2>/dev/null | tail -15 || echo "(无)"
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" "$log2" 2>/dev/null || echo "(无)"

echo
echo "== 判据 =="
[ "$mfs_magic" -eq 1 ] && echo "  [1] 真盘落下 MFS8 超级块 ✅" || echo "  [1] 真盘落下 MFS8 超级块 ❌"
[ "$reboot_ok" -eq 1 ] && echo "  [2] 重启后读回 HELLO.TXT ✅" || echo "  [2] 重启后读回 HELLO.TXT ❌"

[ "$mfs_magic" -eq 1 ] && [ "$reboot_ok" -eq 1 ]