# 安装与工具链

只想了解 CodeQuest，可以直接打开 [在线 demo1](https://akiasprin.github.io/codequest/demo1.html)，或[下载 HTML](demo1.html) 在本地浏览，无需安装本页工具。演示从沙箱创建请求追到 eBPF 表，提供索引、调用图、设计意图和 MCP 等交互视图。3D 图从 cdnjs 加载 three.js，离线时显示说明，其余内容仍可使用；页面不会启动实际构建或模型服务。

## 环境要求

当前支持范围是 Linux/Unix。下面区分项目声明的要求和验收使用的组合，不将本机缓存当作安装前提。

| 组件 | 要求与用途 |
| --- | --- |
| CodeQuest workspace | `Cargo.toml` 声明 Rust **1.95**、edition 2024；`rust-toolchain.toml` 固定到 **1.95.0**。 |
| Rust HIR 提供者 | 与主程序统一使用 **1.95.0**，记录在 `producer.lock.json`；安装脚本显式选择它，即使提供者缓存放在仓库外也保持一致。 |
| 系统构建工具 | Git、Python 3、C/C++ 编译器及可用构建后端。安装脚本使用 Python 标准库；系统包名由发行版决定。 |
| Ladybug | 当前锁定 `lbug = 0.22.3`。C++ FFI 需要支持 **C++20** 的编译器；数据库源码构建路径还需要 **CMake ≥ 3.15** 与 Make 或 Ninja。 |
| Rust 分析 | 被分析项目实际使用的工具链及对应的 **rust-src**；项目若固定 nightly 或其它版本，应准备它自己的组件。 |
| Go 分析 | Go 工具链。完整本地验收含 `go 1.25` 夹具，需要 **Go 1.25 或更新版本**；目标仓库仍以自己的 `go.mod` 为准。 |
| C/C++ 分析 | 项目编译器、compilation database 和 libclang 运行库。已验证组合为 **Clang/libclang 21**，不据此宣称所有其它版本兼容。 |
| 可选工具 | `pdftotext` 用于 PDF 文本层；rustfmt、Clippy 用于开发检查。 |

Ladybug 可能使用预编译数据库，也可能回退到源码构建。预编译路径不取消 Rust/C++ FFI 对编译器的需求。通常不需要另外启动一个数据库服务器。

## 构建主程序和 Rust 提供者

先安装表中的系统依赖，再在仓库根目录执行：

```bash
rustup toolchain install 1.95.0 --profile minimal --component rust-src --component rustfmt --component clippy
cargo +1.95.0 build --release --workspace --locked
RUSTUP_TOOLCHAIN=1.95.0 python3 scripts/setup_rust_hir.py --fetch
```

仓库固定使用 Rust **1.95.0**，并安装 rust-src、rustfmt、Clippy。上面的显式选择不会修改全局默认工具链；HIR 安装脚本从锁定信息读取同一版本，不依赖机器默认版本。被分析项目的编译工具链由该项目自己的配置决定。

首次安装会访问 Cargo registry 与锁定的 rust-analyzer Git 仓库；HIR 安装脚本只修改 `.codequest-tools/` 中自己的 checkout。`--fetch` 负责获取提供者的 Cargo 依赖，不负责安装系统依赖或 rustup 工具链。只有源码与依赖缓存已经备妥，才可省略它进行离线依赖构建。

构建结果包括 `target/release/cq`、`cq-rustc-recorder`、`cq-clang` 与 `cq-store-host`。保留这些辅助程序与主程序的同级布局；仅复制 `cq` 不能代替完整安装。Go 提供者默认在首次 Go 分析时从仓库中的 `providers/go-types/main.go` 构建，因此使用默认路径时应保留仓库布局。

对采用另一套工具链的 Rust 项目，还需为该工具链安装组件。例如：

```bash
# 使用被分析项目实际指定的版本替换 <toolchain>。
rustup component add rust-src --toolchain <toolchain>
```

## 向量数据与提供者位置

```bash
python3 scripts/setup_embeddings.py
```

该步骤可选，下载的文件逐一核对锁定摘要。当前数据是静态 token vectors，不是完整 transformer 推理模型；缺失时系统明确降级为离线随机索引，不能将降级结果当作预训练语义检索。

工具缓存默认在 `.codequest-tools/`，可用下列变量指定已安装的工具：

| 变量 | 指向 |
| --- | --- |
| `CQ_RUST_HIR` | `cq-rust-hir` 可执行文件 |
| `CQ_GO_TYPES` | `cq-go-types` 可执行文件 |
| `CQ_CLANG` | `cq-clang` 可执行文件 |
| `CQ_TOKEN_VECTORS` | 同时包含 `code_tokens.txt` 与 `code_vectors.bin` 的目录 |
| `LIBCLANG_PATH` | libclang 的文件或所在目录，供运行时查找使用 |

不要把操作系统里的任意 `rust-analyzer` 二进制当作 `cq-rust-hir`；CodeQuest 使用带专用导出协议的锁定提供者。

## 常见问题

| 现象 | 核对内容 |
| --- | --- |
| 提供者提示工具链不存在或 rustc 版本不足 | 确认已安装 **1.95.0**；主 workspace、仓库工具链和 HIR 锁定信息应一致。 |
| CMake 或 C++ 编译失败 | 检查 CMake 版本、Make/Ninja、C++20 编译器及标准库；不要只安装 Rust。 |
| 提示 `libclang unavailable` | 安装 libclang 运行库；必要时设置 `LIBCLANG_PATH`，再确认与所分析项目兼容。 |
| Rust 标准库解析缺失 | 给目标项目实际使用的工具链安装 `rust-src`，而不只是给构建 CodeQuest 的工具链安装。 |
| 找不到 recorder、clang 或 StoreHost | 核对辅助二进制与 `cq` 的同级目录；提供者可通过对应变量指定。 |
| `--offline` 缺少 Cargo 依赖 | 先完成在线构建；主 workspace 可用 `cargo +1.95.0 fetch --locked` 准备缓存，HIR 缓存由它自己的安装脚本准备。 |

当前验证命令见 [README 的验证部分](../README.md#验证)。安装说明没有把未执行的干净机器构建称为通过，也不承诺固定首次构建耗时。

## 要求的依据

- [CodeQuest workspace](../Cargo.toml) 与 [HIR 锁定信息](../providers/rust-hir/producer.lock.json)。
- [锁定 rust-analyzer 的 Cargo.toml](https://github.com/samchon/rust-analyzer/blob/2850ecba80311bebd4cdaa9fedc5321533b5b1e7/Cargo.toml)。
- [lbug 0.22.3 构建脚本](https://docs.rs/crate/lbug/0.22.3/source/build.rs) 与 [所带 CMake 配置](https://docs.rs/crate/lbug/0.22.3/source/lbug-src/CMakeLists.txt)。
