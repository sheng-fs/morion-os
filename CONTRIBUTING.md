# 贡献指南

感谢你对 Morion OS 的关注！本文档说明如何参与项目开发、提交 Issue 与 Pull Request。

## 贡献工作流（重要）

所有贡献统一通过「**新建自己的分支 → 推送 → 提交 PR → 检查通过 → 合并**」完成，**不要直接向 `main` 分支推送代码**。

```
1. 从 main 切出你自己的分支
       ↓
2. 在该分支上开发并提交
       ↓
3. 推送你的分支到远端
       ↓
4. 提交 Pull Request 到 main
       ↓
5. 通过 CI 检查 + 维护者 review
       ↓
6. 合并到 main
```

```bash
# 1. 确保基于最新的 main
git checkout main
git pull origin main

# 2. 新建自己的功能分支（用简短、描述性的名字）
git checkout -b feat/your-feature
# 或修复缺陷: git checkout -b fix/your-bug

# 3. 开发并提交（遵循下方的提交规范）
git add <相关文件>
git commit -m "feat(boot): 新增亚克力主题渲染"

# 4. 推送自己的分支到远端
git push -u origin feat/your-feature

# 5. 在 GitHub 上打开 Pull Request，等待检查与 review
```

> 合并前需满足：CI 检查全部通过、至少一位维护者 review 通过。合并由维护者执行。

## 快速开始

Morion OS 是基于 Rust 的 UEFI 引导加载器 + 微内核项目，当前处于**重新设计与重写阶段**。旧代码已归档到 `legacy` 分支，主线 `main` 重新开始。

### 环境准备

- **Rust nightly** 工具链（由根目录 `rust-toolchain.toml` 固定）
- 目标三元组：`x86_64-unknown-uefi`（引导器）、`x86_64-unknown-none`（内核）
- 构建依赖：`nasm`、`qemu`、`xorriso`、`mtools`、`edk2-ovmf`

```bash
# 一键检查并安装 Rust 工具链与依赖提示
make setup

# 构建完整 ISO 镜像
make iso

# 在 QEMU 中运行
make run          # 需要 KVM
make run-nokvm    # 无硬件虚拟化环境
```

其他常用目标见 `make help`。

## 目录结构约定

项目为 Rust workspace，当前包含三个 crate：

| 路径 | 说明 |
|------|------|
| `boot/` | UEFI 引导加载器（morion-boot） |
| `kernel/` | 微内核，最小可信基（morion-kernel） |
| `kernel_test/` | 引导器联调用测试内核（临时） |

资源归档约定：

- 引导期资源 → `boot/loader/resources/`（动画、背景、图标、Logo、进度条、闪屏）
- 系统全局资源 → `resources/system/`（设备/文件/服务图标、系统 Logo、终端背景）
- 引导配置 → `boot/loader/`（`loader.conf`、`theme.toml`、`entries/`）

> 新增资源请按用途放入对应目录，不要散落在根目录；修改资源路径时同步更新构建配置（`Makefile`、`boot/` 中的配置）。

## 分支规范

- `main`：主线，全新重写的代码（受保护，不直接推送）
- `legacy`：旧代码归档分支，只读
- 功能分支：`feat/xxx`、`fix/xxx`，完成后通过 PR 合并回 `main`

## 提交规范

使用 [Conventional Commits](https://www.conventionalcommits.org/) 风格：

```
<type>(<scope>): <简要描述>
```

常用 `type`：

| type | 说明 |
|------|------|
| `feat` | 新功能 |
| `fix` | Bug 修复 |
| `refactor` | 重构 |
| `docs` | 文档 |
| `ci` | 构建 / CI |
| `chore` | 杂项维护 |

示例：`feat(boot): 新增亚克力主题渲染`、`fix(kernel): 修复缺页处理`。

## 代码风格

- 提交前确保通过检查：

```bash
make fmt      # 代码格式
make clippy   # Clippy 检查（-D warnings）
make check    # 编译检查
```

- 注释使用中文，与现有代码保持一致。

## Issue 规范

报告问题或提出建议前，请先搜索是否已有相关 Issue。提交时选择对应模板：

- **Bug 报告**：`[Bug]` 前缀，附带复现步骤、环境信息与日志
- **功能请求**：`[Feat]` 前缀，说明背景动机与方案

### 标签规划

| 类型 | 标签 |
|------|------|
| 缺陷 / 功能 / 文档 / 重构 | `bug` / `feat` / `docs` / `refactor` |
| 性能 / 杂项 / 测试 | `perf` / `chore` / `test` |
| 组件归属 | `boot` / `kernel` / `ui-assets` / `security` / `ci` |
| 优先级 | `priority-high` / `priority-medium` / `priority-low` |
| 状态 | `good-first-issue` / `help-wanted` / `question` / `wontfix` |

完整标签定义见 [.github/labels.yml](./.github/labels.yml)。

## Pull Request 规范

1. 从 `main` 切出你自己的功能分支进行开发（见上方「贡献工作流」）
2. 遵循提交规范与代码风格
3. 使用 PR 模板填写变更说明、关联 Issue 与测试情况
4. 确保 CI 检查（`check` / `fmt` / `clippy`）全部通过
5. 由维护者 review 通过后合并到 `main`

## 许可证

贡献的代码默认遵循项目的 [MIT 许可证](./LICENSE)。
