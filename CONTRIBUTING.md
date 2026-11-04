# 贡献指南

感谢参与 Morion OS。本文只保留「怎么快速跑起来、怎么提 PR」的必要信息；架构与接口细节见
[docs/architecture.md](./docs/architecture.md) 和 [docs/dev-reference.md](./docs/dev-reference.md)。

## 60 秒上手

依赖：Rust nightly（`rust-toolchain.toml` 已固定）、`nasm`、`qemu`、`xorriso`、`mtools`、`edk2-ovmf`。

```bash
make setup        # 检查工具链并提示缺失依赖
make iso          # 构建可启动 ISO
make run          # QEMU + KVM；无 KVM 用 make run-nokvm
```

`make help` 可看全部目标。

## 提交流程

`main` 受保护，**不要直接 push**。所有改动走分支 + PR：

```bash
git switch main && git pull
git switch -c feat/your-feature     # 或 fix/、docs/、refactor/
# 开发……
git add -p
git commit -m "feat(boot): 新增亚克力主题渲染"
git push -u origin feat/your-feature
```

然后在 GitHub 打开 PR：模板会自动带出变更类型、关联 Issue 和测试清单。合并条件：CI 全绿 +
至少一位维护者 review；合并由维护者执行。

## 提交信息

使用 [Conventional Commits](https://www.conventionalcommits.org/)，格式 `<type>(<scope>): <描述>`：

| type | 用途 | type | 用途 |
|------|------|------|------|
| `feat` | 新功能 | `test` | 测试 |
| `fix` | Bug 修复 | `build` | 构建 / 依赖 |
| `refactor` | 重构（不改行为） | `ci` | CI |
| `docs` | 文档 | `chore` | 杂项维护 |
| `perf` | 性能 | `style` | 纯格式 |

示例：`feat(xhci): 支持多控制器与 USB HID 键盘`、`fix(init): 连续失败后放弃重启`。

一个 PR 尽量只做一件事；提交信息写清楚「为什么」，不必罗列改了哪几行。

## 合并前自检

```bash
make fmt && make check && make clippy   # 与 CI 相同的三项门禁
```

- **内核 / 文件系统 / 驱动改动**：再跑一次 `bash scripts/fs-regress.sh`（QEMU 全量自测，本地 KVM
  约 6 分钟；CI 每周定时跑，也可在 Actions 里手动触发）。
- **用户可见行为变更**：同步 `README.md`（英文主）、`README.zh-CN.md` 等各语言版本、相关 `docs/` 与 `CHANGELOG.md`。
- **改了用户态服务**：必须 `make iso` —— 服务 ELF 会被打进 ESP 和 FAT32 根盘。
- **切换构建变体**（`NOGUI=1` / `SAFE=1`）前先 `cargo clean`，避免复用上一个变体的产物。
- **真机没有串口**：用 `dmesg /boot.log` 把日志写到 U 盘带出来。

## 分支与目录

- `main`：主线，受保护；`legacy`：旧代码归档，只读。
- 功能分支：`feat/xxx`、`fix/xxx`、`docs/xxx`、`refactor/xxx`。
- 主要目录：`boot/`（UEFI 引导器）、`kernel/`（微内核）、`user/`（用户态服务与库）、
  `docs/`（架构 / 接口 / 路线图）、`scripts/`（构建与回归脚本）。
- 新增资源按用途放：引导期资源进 `boot/loader/resources/`，系统资源进 `resources/system/`；
  不要散落在根目录，改路径时同步更新 `Makefile` 与引导配置。
- 注释用中文；格式直接交给 `cargo fmt`，不要手写对抗 rustfmt 的排版。

## Issue

先搜索是否已有同类 Issue，再选模板：

- **Bug**：复现步骤、环境（真机 / QEMU、构建变体）、`dmesg` 日志；
- **Feature**：使用场景、期望行为、可选方案。

标签由维护者按 [.github/labels.yml](./.github/labels.yml) 添加，提交时不用纠结分类。

## 许可证

本项目采用**按模块许可**，提交前请确认你改的目录对应哪个协议：

- 核心（`boot/`、`kernel/`、`user/`）：MIT；
- 飞地实现（`enclave/`）：Apache-2.0（含专利授权）；`enclave/abi/` 仍为 MIT；
- 第三方驱动 / 固件：保留原协议，不要把 GPL 代码提交进主仓库。

提交即表示你同意对应目录的 SPDX 协议。新增文件请加 SPDX 头（如 `// SPDX-License-Identifier: MIT`），
第三方代码注明来源、原协议与 commit。完整映射见 [docs/licensing.md](./docs/licensing.md)。
