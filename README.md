# jvmsense v4

jvmsense v4 是一个 Windows/JVM 导向的 Rust 2021 workspace，核心目标是在启动 Java 应用时让应用需要的字节码不以可分析的完整文件形式落盘。

当前主 crate 是 `apps/core` 中的 `jvmsense-core`。它实现了一条 **hollow-path 虚拟文件系统**路径：

- 磁盘上的每个 artifact 只存在为 **0 字节 placeholder**；
- 真实字节保存在当前进程内存；
- Windows native hooks 拦截 Java/JVM 对 placeholder 的打开、长度查询和读取；
- Java 侧看到的仍是普通路径、普通 jar 与普通类加载流程。

## Fabric 正确性路径：launch 前注入

jvmsense 的默认 Fabric 路径不是运行后注入，而是在创建 JVM 之前完成准备：

1. 读取或下载 game、Fabric Loader、runtime mod；
2. 解析 `fabric.mod.json` 与嵌套 `jars`；
3. 递归展开嵌套 Fabric mod，并把非 Fabric 嵌套 jar 变成 library classpath entry；
4. 从父 jar 中移除嵌套 jar 条目和 `jars` 声明，避免 Fabric 在运行中提取 `.fabric/processedMods`；
5. 将 game、loader、mods、libraries mount 到 VFS；
6. 生成 classpath、`fabric.addMods` 与 `fabric.runtimeMappingNamespace=intermediary` 等 JVM/Fabric 属性；
7. 创建 JVM 并让 Fabric/Knot 正常发现 mod；
8. Mixin 在目标类 **第一次加载**时完成变换。

因此，Mod Menu 这类真实 Fabric mod 不需要 JVMTI redefine。JVMTI 只保留为显式 fallback，用于目标类已经被加载、无法再依靠首次类加载流程处理的场景。

架构细节见 [docs/architecture.md](docs/architecture.md)。

## 仓库结构

```text
apps/core          jvmsense-core 库与 jvmsense CLI
docs               架构与工程说明
examples           生成型测试/运行夹具；手写源码片段会被跟踪
spikes/FINDINGS.md Windows/JVM 探针结论
.agents/skills     项目内 Codex skills
```

`spikes/` 中的可执行探针刻意不属于 Cargo workspace，也不作为产品源码提交。

## 构建与检查

本项目当前面向 Windows JVM instrumentation 行为；在 Windows 上验证最完整。

```powershell
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

真实 JVM / Fabric / Mixin 集成测试默认标记为 ignored，会下载 JDK 与测试夹具并启动真实 Java 进程：

```powershell
cargo test --locked --test launch_fabric_mixin -- --ignored --nocapture
```

该测试验证：

- Fabric Loader 和 Mod Menu 均为内存镜像；
- 目标类首次加载时 Mixin 方法已经存在；
- 全程未启用 JVMTI redefine；
- placeholder 保持 0 字节；
- 嵌套 mod 没有被提取到 `.fabric/processedMods`。

## 版本控制边界

Git 只跟踪源码、测试、锁文件、文档和项目配置。以下内容属于本地生成物，不应提交：

- `target/`
- `.codegraph/`
- `.jdk/`
- 下载的 Minecraft / Fabric / Mod Menu / remapper 夹具；
- `examples/` 下的运行输出；
- `apps/core/.fabric/`、`apps/core/logs/`、`apps/core/config/`;
- `spikes/` 中的一次性探针工程，除 `FINDINGS.md` 外。
