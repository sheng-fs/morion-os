#!/usr/bin/env bash
# =============================================================================
# ra-check.sh — 给 rust-analyzer 用的「逐 crate 正确 target」检查入口
# =============================================================================
# 为什么需要它:
#   rust-analyzer 默认对工作区跑**宿主 target** 的 `cargo check`。本仓库三个目标互不
#   相同 (kernel: x86_64-unknown-none / boot: x86_64-unknown-uefi / 用户态: 自定义 json
#   target + build-std), 没有单一 target 能覆盖全工作区 —— 宿主 target 下 no_std 的
#   crate 会在 crate 根 main.rs 上报 `#[panic_handler] required` (内核还多一个
#   `no global memory allocator found`), Problems 面板因此常年一片"假红"。
#   Makefile 的 `make check` (544-572 行) 早就按正确 target 逐个检查; 本脚本把同一组
#   命令搬过来, 只多一个 `--message-format=json` —— rust-analyzer 从 stdout 读 cargo
#   JSON 诊断流, 把**真实**诊断照常画进编辑器, 误报消失。
#
# 与 `make check` 的对应关系:
#   下面 cargo 命令逐条对应 Makefile check 目标的 5 条 (另有 kernel_test 一条, 见下),
#   **改了那边记得同步这边**。
#
# 约定 / 注意:
#   - stdout 只允许出现 cargo 的 JSON 行 (rust-analyzer 只按行解析 stdout);
#     脚本自己的提示一律走 stderr。
#   - cargo 因真实错误退出码非 0 是常事, rust-analyzer 只看 stdout, 故最后恒 `exit 0`。
#   - 干净树上 include_bytes! 的生成物还不存在 (kernel ← build/user/srv/*.elf,
#     boot ← boot/loader/morion-kernel.elf), 先跑一次 `make check` (或 `make`) 生成,
#     否则会看到 include_bytes! 的"读不到文件"误报。这里不自动构建: 检查要保持快。
#
# IDE 侧接线: .vscode/settings.json 的 `rust-analyzer.check.overrideCommand`。
# =============================================================================

# 切到仓库根 (rust-analyzer 一般在根目录调用, 但不依赖它的 cwd)。
cd "$(dirname "$0")/.." || exit 0

# 保证无论如何都能找到 cargo: IDE 启动 rust-analyzer 时的 PATH 往往不含 ~/.cargo/bin,
# 缺了 cargo 会让 overrideCommand 整体失败 (诊断反而全无)。
export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/bin:/bin:$PATH"

# 与 Makefile 相同的编译期环境变量 (见 Makefile 28-44 行): 版本串与 nightly 门。
export RUSTC_BOOTSTRAP=1
export MORION_BUILD="${MORION_BUILD:-$(git rev-parse --short HEAD 2>/dev/null || date +%Y%m%d)}"

USER_TARGET="user/x86_64-morion-user.json"

# 干净树提醒 (stderr, 不污染 JSON 流)。
if [ ! -f boot/loader/morion-kernel.elf ] || [ ! -d build/user/srv ]; then
  echo "ra-check: 提示: 缺少 include_bytes! 的生成物 (boot/loader/morion-kernel.elf / build/user/srv/*.elf)," \
       "先跑一次 \`make check\` 生成 —— 否则编辑器里会有 include_bytes! 的误报" >&2
fi

# --- 内核 (morion-kernel) ---
cargo check --package morion-kernel --target x86_64-unknown-none --message-format=json

# --- 用户态 (自定义 json target + build-std; 三个 crate 共用同一组参数) ---
cargo check --package morion-srv --target "$USER_TARGET" -Z json-target-spec \
  -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem \
  --message-format=json
cargo check --package morion --target "$USER_TARGET" -Z json-target-spec \
  -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem \
  --message-format=json
cargo check --package morion-hello --target "$USER_TARGET" -Z json-target-spec \
  -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem \
  --message-format=json

# --- 引导器 (morion-boot) ---
cargo check --package morion-boot --target x86_64-unknown-uefi --message-format=json

# --- 早期测试内核 (morion-kernel-test) ---
# `make check` 不管它 (已弃用), 但它是 workspace 成员: 不在这里检查的话, 它的
# main.rs 会继续在 Problems 面板里报宿主 target 的 `#[panic_handler] required` 假红。
cargo check --package morion-kernel-test --target x86_64-unknown-none --message-format=json

# cargo 的非零退出码对 rust-analyzer 无意义 (诊断已在 stdout), 恒成功退出。
exit 0