# P0 Spike 结果

`D:\jvmsense_v4\spikes\` 下的探针用于在写正式工程前判定架构成立与否。
每个 spike 都是独立的最小 Rust 可执行文件，直接跑，不依赖 workspace。

环境：Rust 1.98.1 / JDK 25（javac，用于夹具）/ 捆绑 JRE `jdk-21.0.10+7-jre`（位于 `D:\Project\jvmsense3\jvmsense3-testing\`）。

---

## V1 — `RandomAccessFile` hook 能否服务 `ZipFile`？ ✅ **PASS（go 信号）**

**结论**：只 hook 7 个 native，`new java.util.zip.ZipFile(0字节占位文件)` / `new JarFile` /
`getManifest` / 70000 字节 `readAllBytes` **全部正确从内存返回**。空心路径 VFS 架构成立，不需要
`FileDispatcherImpl` / `nio.dll` / zipfs hook 族。

**实测 hook 命中**（`spikes/v1-raf-zipfile` 输出）：

```
open0.hit        1
length0.hit      1
seek0.hit        8
readBytes.hit    8
```

即 `ZipFile` 只走 `open0 → length0 → seek0 → readBytes0`。`read0`（单字节）、`getFilePointer`、
`close0` 未被 `ZipFile` 触及（但仍需 hook，因为其他调用方会用）。

### 两个必须记住的坑

1. **符号名与直觉不符**，必须从 `java.dll` 导出表核实，不能猜：

   | 想当然 | 实际 |
   |---|---|
   | `Java_java_io_RandomAccessFile_readBytes` | `Java_java_io_RandomAccessFile_readBytes`**`0`** |
   | `Java_java_io_RandomAccessFile_close0` | **不存在**；关闭走 `Java_java_io_FileDescriptor_close0`，且其 receiver 是 `FileDescriptor` 对象本身，不是流 |

   工具：`python spikes/v1-raf-zipfile/dump_exports.py <java.dll> <子串>`（纯 stdlib PE 导出表解析，
   无需 dumpbin）。捆绑 JRE 的 `java.dll` 共 292 个导出，其中 `RandomAccessFile` 10 个、
   `FileDescriptor` 5 个、`FileInputStream` 8 个。

2. **`open0` 不能短路**。必须先调用原始 `open0`，让 JDK 真的打开那个 0 字节占位文件（从而
   `this.fd` 拿到有效描述符），之后才把虚拟字节挂到该 fd 上。短路会导致 fd 恒为 `-1`，
   `ZipFile` 报 `IOException: Stream Closed`。命中统计里 `open0.hit` 是"原始 open 成功后挂钩"的次数。

### 实现要点

- 用 fd 作键在 Rust 侧维护 `(placeholder path, position)`；`RandomAccessFile` 显式 `seek`，
  所以只需记录当前位置，不需要模拟文件指针。
- **`length0` 必须返回虚拟长度**。占位文件是 0 字节，返回 0 会让 `ZipFile` 立即放弃。
- `readBytes0` 到 EOF 必须返回 `-1`（不是 0）——`ZipFile` 靠这个停止读取。
- 所有 JNI 字段访问走原始 `JNIEnv` 函数表（`jni-sys` 的 `JNIEnv` 本身就是
  `*const JNINativeInterface_`，`table(env) = &**env`）。

---

## V2 — 会话目录的 `toRealPath()` 往返 ✅ **PASS（原 R2 是虚惊）**

**结论**：Java 的 `Paths.get(p).toRealPath().toString()` 在三个候选目录上**与输入字符串完全相等**
（精确匹配，不只是归一化后匹配）：

| 目录 | toRealPath 结果 |
|---|---|
| `C:\Users\winxp\AppData\Local\Temp\jvmsense-v2-realpath` | 原样返回 |
| `D:\jvmsense_v4\.session-test` | 原样返回 |
| `C:\Users\winxp\AppData\Local\jvmsense` | 原样返回 |

fabric-loader 的 `ModDiscoverer` 断言 `path.equals(normalizeExistingPath(path))`（内部走
`toRealPath`）因此在这些目录上成立。

**V1 里那个 `MISMATCH` 是探针自身的 bug**：`std::fs::canonicalize` 在 Windows 上**总是**加
`\\?\` 扩展长度前缀（`\\?\C:\Users\...`），所以朴素的字符串比较必然不等。是探针错了，不是路径有问题。
会话目录本身也确实不是 reparse point（`%LOCALAPPDATA%` / `%TEMP%` / `D:\jvmsense_v4` 属性均为
`0x10` = `FILE_ATTRIBUTE_DIRECTORY`）。

**对 v4 的指导**：启动时的目录不变式检查要**用 Java 的 `toRealPath` 语义**，或用
`dunce::canonicalize`（会剥掉 `\\?\`），不要直接用 `std::fs::canonicalize` 做字符串比较。
另外 `C:\jvmsense` 确实**是** reparse point——这类检查仍有价值，只是要用对 API。

**JNI 注意**：`Paths.get` 和 `Path.toRealPath` 都是 varargs。传 **null 数组**会让 JDK 在
`<parameter2>.length` 上抛 NPE，必须构造真正的空数组
（`env.new_object_array(0, "java/lang/String", JObject::null())`）。

---

## V6 — `ServiceLoader` 能否发现自定义 `FileSystemProvider`？ ✅ **PASS**

把 provider 打成**真实 jar**（含 `META-INF/services/java.nio.file.spi.FileSystemProvider`）放在
`-cp` 上，`FileSystemProvider.installedProviders()` 就能发现它：

```
installedProviders() count = 4
    - sun.nio.fs.WindowsFileSystemProvider  scheme=file
    - jdk.nio.zipfs.ZipFileSystemProvider   scheme=jar
    - jdk.internal.jrtfs.JrtFileSystemProvider  scheme=jrt
    - memfs.MemFsProvider  scheme=memfs        ← 我们的
FileSystems.newFileSystem(memfs:///) -> MemFs[memfs:///]   ✅
FileSystems.getFileSystem(memfs:///)  -> MemFs[memfs:///]   ✅
Path.toFile() 正确抛 UnsupportedOperationException          ✅
```

**对 v4 的指导**：
- 计划里"发布一个真实、无敏感的 `assets/bootstrap.jar`"是**必需且充分**的。`DefineClass` 确实不会注册
  `META-INF/services`，所以 provider 必须走真实 jar 的系统类路径。
- **[5] 这条是最重要的**：自定义 FS 的 `Path.toFile()` **正确抛异常**。这从正面证实了计划的核心约束——
  任何指望 `path.toFile()` 工作的代码（fabric-loader 有 10 处 `new ZipFile(path.toFile())`）
  **无法**由自定义 FileSystemProvider 满足，因此必须用空心路径方案。
- 代码在 `spikes/v4v6-provider/`（`java/src/memfs/*.java` + `Probe.java`）。作者刻意让 `MemFsPath.toFile()`
  抛异常而不是继承默认实现，以便把这个约束固化成可执行的检查。

---

## V4 — `RawNativeLibraries_load0/unload0` 是否存在？ ✅ **存在（符号确认）**

从导出表核实，**JDK 21.0.10 与 JDK 25 都有**：

```
Java_jdk_internal_loader_RawNativeLibraries_load0
Java_jdk_internal_loader_RawNativeLibraries_unload0
Java_jdk_internal_loader_NativeLibraries_findBuiltinLib
Java_jdk_internal_loader_NativeLibraries_load
Java_jdk_internal_loader_NativeLibraries_unload
```

捆绑 JRE 21 的 `java.dll` 共 292 个导出，Zulu 25 的是 286 个，两者都含上述 5 个符号。
**结论**：v4 必须两个族都 hook（`RawNativeLibraries_*` 与 `NativeLibraries_*`），
否则 `System.load` 会绕过注册表——这正是前代的缺口（[[jvmsense-v4-hollow-path-vfs]] 里记的 R4）。

---

## V7 — `ristretto_classfile` 的 `max_stack` 是否正确？ ❌ **确认有系统性 bug**

**结论：`ristretto_classfile` 0.29.0 的 invoke 系列 `stack_delta` 完全忽略返回值。**
`6/9` 种 invoke 形式算错。**v4 必须自己实现 `max_stack`/`max_locals` 计算，不能信任这个 crate 给 Code 属性定尺寸。**

### 根因（`src/attributes/instruction.rs:1352-1370`）

```rust
Instruction::Invokevirtual(method_index)
| Instruction::Invokespecial(method_index)
| Instruction::Invokestatic(method_index)
| Instruction::Invokedynamic(method_index) => {
    let (_class_index, name_and_type_index) = constant_pool.try_get_method_ref(*method_index)?;
    let (_name_index, descriptor_index) = constant_pool.try_get_name_and_type(*name_and_type_index)?;
    let method_descriptor = constant_pool.try_get_utf8(*descriptor_index)?;
    let (parameters, _return_type) = FieldType::parse_method_descriptor(method_descriptor)?;  // ← 返回值被丢弃
    let delta = -i16::try_from(parameters.len())?;
    if matches!(self, Instruction::Invokestatic(..)) || matches!(self, Instruction::Invokedynamic(..)) {
        delta
    } else {
        delta.saturating_sub(1)   // "Subtract 1 for the object reference"
    }
}
```

正确公式是 `delta = return_slots - (params + receiver)`，而它算的是 `-params [-1]`。`_return_type` 解析出来了却没用。

### 实测（`spikes/v7c-scope`）

| 指令 | 实际 | 应为 | |
|---|---:|---:|---|
| `invokestatic ()V` | 0 | 0 | ok |
| `invokestatic ()Ljava/lang/String;` | 0 | 1 | **错** |
| `invokestatic (I)I` | -1 | 0 | **错** |
| `invokestatic ()J`（2 槽返回） | 0 | 2 | **错** |
| `invokevirtual ()V` | -1 | -1 | ok |
| `invokevirtual ()I` | -1 | 0 | **错** |
| `invokevirtual ()Ljava/lang/String;` | -1 | 0 | **错** |
| `invokespecial ()V` | -1 | -1 | ok |
| `invokespecial ()Ljava/lang/Object;` | -1 | 0 | **错** |

**规律**：只有返回 `void` 时才对。任何非 void 返回都错（差 -1，long/double 差 -2）。

### 三条重要推论

1. **前代的 `ensure_code_max_stack(2)` 确实是必需的**，不是代码洁癖。前代只对两个构造函数做 `max(_, 2)`，
   那是**恰好够用**（那两个 ctor 的构造恰好让真实峰值是 2），但**不是通用修复**——v4 生成/重写的类大得多，
   必须系统性地解决。
2. **crate 自己的 `verify()` 会接受错误的值**。V7 里 `max_stack=1`（错）和 `max_stack=2`（对）**都能通过 `verify()`**。
   所以"生成后跑一次 verify"**不能**当作 `max_stack` 正确性的保障——这正是它危险的地方。
   这也解释了前代为什么没被发现：没有测试能抓住它。
3. **`max_locals` 也不可信**，虽然本次未直接证伪（`max_locals_index` 只看 `Iload/istore/...` 的索引，
   逻辑上不依赖返回值），但既然要为 `max_stack` 写自己的实现，`max_locals` 一并自己算
   （还要正确处理 `long`/`double` 占 2 槽、以及 ctor 的 `this`）。

### 对 v4 计划的修改

- 计划里 `bytecode/` 模块"独立的 `max_stack`/`max_locals` 计算"从**可选回退**升为**必做组件**，
  并且要用它**替代**（而非校验）ristretto 的计算。
- Layer 1 测试里"独立实现必须与 ristretto 一致"这条**要反过来**：应当断言**与 ristretto 不一致**，
  并把这个不一致作为回归测试固定下来（否则将来有人"修好"了我们的实现反而会被测试判为失败）。
- 建议向上游报 issue（返回值被丢弃是明确的实现缺陷）。

---

## V3 — nio / WinNTFileSystem hook 族 ✅ **已完成并验证**

`Files.*` 与 `File.length()` 原先返回 0，现已全部从内存正确读取。实测（`tests/read_path_probe.rs`，对 0 字节占位）：

| 路径 | 结果 |
|---|---|
| `new ZipFile(file).getInputStream(entry)` | OK **3088** 字节 |
| `new JarFile(file).getInputStream(entry)` | OK **3088** 字节 |
| `URL("jar:file:...!/entry").openStream()` | OK **3088** 字节 |
| `Files.readAllBytes(path)` | OK **2924** 字节 |
| `Files.size(path)` | OK **2924** |
| `File.length()` | OK **2924** |
| `RandomAccessFile.readFully(byte[])` | OK length **2924** |

共 18 个 hook（原 7 + `WinNTFileSystem` 2 + `FileDispatcherImpl` 5 + `WindowsNativeDispatcher` 4）。

### V3 踩到的四个坑（都很贵，务必记住）

1. **`nio.dll` 是懒加载的**。不像 `java.dll` 由 `jvm.dll` 在创建时载入，必须显式
   `LoadLibraryW("<java.home>in
io.dll")`，否则 `GetProcAddress` 全部失败，
   hook 永不安装。

2. **`FileDescriptor.fd` 在 Windows 上恒为 -1**。它是 CRT 描述符；JVM 打开的所有文件
   都走 Win32 handle 路径，真正的值在 **`FileDescriptor.handle`（long）**。
   用 `fd`(int) 做键会记录到 -1 且永远匹配不上。

3. **`Files.*` 根本不走 `FileDispatcherImpl` 打开文件**。它走
   `sun.nio.fs.WindowsChannelFactory` → `WindowsNativeDispatcher.CreateFile0` 拿 handle，
   再包成 `FileChannelImpl` 用 `FileDispatcherImpl` 读。所以只 hook `FileDispatcherImpl`
   时，读的 handle 从没被登记过——这正是装了 nio hook 之后 `Files.size` 仍返回 0 的原因。
   **`CreateFile0` 是路径唯一可见的地方**（参数是原生 `LPCWSTR`），登记必须在那里做。

4. **`Files.size` 既不与 `FileDispatcherImpl.size0` 也不与 `GetFileInformationByHandle0` 同路**，
   而是 `WindowsFileAttributes.get` → **`GetFileAttributesEx0(long path, long buffer)`**。
   这是最好的 hook 点：路径以原生宽字符串传入，且 buffer 是调用方随后读取 size 的地方，
   所以既不需要 handle 映射也不需要伪造返回值，直接在 buffer 里改就行。
   size 字段偏移：`nFileSizeHigh` @28，`nFileSizeLow` @32（从
   `WindowsFileAttributes.fromFileAttributeData` 字节码读出）。

5. **文件位置属于"打开的文件"，不属于路径**。`RandomAccessFile` 与单独打开的
   `FileChannel` 是两个独立的打开文件、两个独立位置。共用一个游标会导致
   `ZipFile` 走完 jar 后把位置留在 2511，随后 `Files.readAllBytes` 从那里续读，
   返回 `2924-2511=413` 字节。**两族必须分命名空间**：RAF 按路径，nio 按 handle。

### 已知未覆盖

**应用类加载器的 `getResourceAsStream` 对类路径条目返回 null**，而同一路径上
`JarFile.getJarEntry` 能查到条目。原因是 `URLClassPath$JarLoader` 缓存的是**首次打开时
的文件系统视图**（0 字节占位），此后不再重新解析。这是类加载器的 URL 缓存行为，
**不是字节服务的缺口**。Fabric 取类字节走 `KnotClassDelegate`、取资源走 `JarFile`/`ZipFile`，
都不受影响。

---

## 真实 Minecraft `af$2` blocker 隔离 — ✅ **不是 hollow VFS 缺口**

40 个 crash report 均重复 `NoClassDefFoundError: af$2`，但源 `client.jar` 中确实存在
`af$2.class`。新增回归测试 `apps/core/tests/minecraft_inner_class.rs` 只挂载真实
client.jar、创建真实 JVM，然后分别验证：

- `JarFile.getJarEntry("af$2.class")` ✅ 能看到 entry；
- 系统 class loader 的 `Class.forName("af$2")` ✅ 能加载类；
- placeholder 全程为 0 字节。

因此剩余 blocker 在 Fabric 的 remap / Mixin / Knot classloader 路径，不在字节服务层。
当前 `client.jar` 仍是 official/obfuscated namespace（无 `net/minecraft/class_*` entry），
而 `launch/fabric.rs` 的前提是输入已经 remap 到 intermediary；V5 是下一步。

## V5 — official→intermediary 与 Mixin/refmap 烘焙 — ✅ 完成

正式实现位于 `apps/core/src/remap/`，并接入 hollow VFS/Fabric 启动：

- `official_to_intermediary()` 使用 Fabric TinyRemapper 0.14.1 的内存输出回调，
  helper 将最终 jar image 写入 stdout；Rust 读入内存，不写 remap 产物文件。
- `intermediary_mixin_mod_to_named()` 使用 TinyRemapper `MixinExtension`，将真实
  Mod Menu 13.0.4 的 Mixin/refmap 目标从 intermediary 烘焙为 named，同样不落盘。
- `ArtifactStore::insert_memory()`、`VirtualFileSystem::mount_memory()` 使内存产物
  内容寻址后直接进入 hollow VFS。
- `FabricApplication::mount_remapped()` 生成 Fabric/Knot classpath 布局。
- CLI 支持 `--remap-official --mappings <PATH> --remapper <PATH>`。

验证命令均已通过：

```powershell
cargo test --locked --test minecraft_official_to_intermediary -- --ignored --nocapture
cargo test --locked --test mixin_refmap_baking -- --nocapture
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

关键实测结果：

- official `client.jar` 28,335,587 bytes → 内存 intermediary image 29,663,827 bytes；
- `af` → `net/minecraft/class_156`，`af$2` → `net/minecraft/class_156$2`；
- Fabric Loader 0.17.0 / Knot 初始化成功，`Class.forName("net.minecraft.class_156$2", true, KnotClassLoader)` 成功；
- hollow VFS trace 命中 1947 次，intermediary placeholder 始终为 0 字节；
- Mod Menu Mixin annotation 由 `net/minecraft/class_442` 烘焙为
  `net/minecraft/client/gui/screen/TitleScreen`，intermediary 目标不再残留。

映射与 TinyRemapper 属于工具链而非游戏字节，缓存在 `examples/minecraft/tools/`。

| # | 内容 | 状态 |
|---|---|---|
| V5 | 离线 remap + Mixin 烘焙对真实 mod 是否产出可用字节码 | ✅ 完成，见上 |
| V8/V9 | fabric-loader 版本漂移 / Mixin 帧计算 | ✅ 升级到 loader 0.18.4 / ASM 9.9 / Mixin 0.17 并验证 |

---


## V6-RT — 运行时 Fabric 内存注入 — ✅ 核心路径已验证

V6 在 V5 的 official→intermediary、hollow VFS 基线上增加了运行时注入：

- `VirtualFileSystem::mount_memory_runtime()`：启动后仍可通过 `&self` 挂载内存 artifact；只有 0 字节 placeholder 落盘。
- `runtime::parse_runtime_mod()`：内存解析 `fabric.mod.json`、Mixin config、nested jars；显式报告 AccessWidener、Accessor/Invoker Mixin。
- `runtime::relaxed_mixin_config_jar()`：生成 `required:false` 的注入配置，绕过 Mixin 对“目标类已加载”的硬拒绝。
- `runtime::jni`：复用 Knot `addToClassPath`、`defineClassFwd`、`getPostMixinClassByteArray`、`FabricLoaderImpl.addMod`。
- `native::jvmti`：直接绑定 JVMTI 1.2 的 `AddCapabilities`/`RedefineClasses`，真实 JVM 重定义测试通过。
- `RuntimeInjector` ASM helper：把 Mixin 新增方法移动到 `JvmsenseStaticHandlers_*`，重写调用点后再走 JVMTI；转换只存在于内存。

### 自动化验证结果

```powershell
cargo test --locked --test minecraft_official_to_intermediary -- --ignored --nocapture
cargo test --locked --test runtime_mixin_redefine -- --ignored --nocapture
cargo test --locked --test runtime_jvmti_redefine -- --ignored --nocapture
cargo test --locked
```

已实测：

- Mod Menu 13.0.4 的 `onInitializeClient` 在预初始化 Knot 上完整执行并返回；
- 至少一个 Mod Menu Mixin 真实改写目标类（`adjustRealmsHeight` / `onRender` handler 可见）；
- 已加载的 `net.minecraft.class_442` 先加载、后注册 Mixin、再经 StaticHandlers + JVMTI 重定义成功；
- refmap 通过 hollow VFS 从内存读取，所有 placeholder 在泄漏检查中均为 0 字节；
- `fabric.development` 在 V6 测试布局中明确断言未设置；
- 真实 Mod Menu 计划报告 `AccessorGridWidget` 为需要能力声明的 Accessor Mixin。

### 与 RuntimeFabricMod `memory://` 的差异

V6 没有引入 `memory://` URL handler，而是复用了项目既有的 hollow VFS：Mixin config/refmap/class 仍以普通资源名解析，但对应 jar 是 0 字节 placeholder，字节读取全部由 native hooks 从内存返回。二者保持同一安全目标（不落盘），
且不需要修改 Mixin service；后续若要支持跨进程 DLL 注入，再补 `memory://`/自定义 service。

### 未完成边界

- AccessWidener 仍只检测、不执行。
- 已加载目标上的 Accessor/Invoker Mixin 仍需 StaticHandlers 之外的专用转换。
- 跨进程 DLL/JVMTI 注入与 Windows DLL 载荷封装属于 V7。
## 环境备忘

- `jni` crate：crates.io 的 **0.21.1 没有** `JNIVersion::V21` / `EnvUnowned` / `Outcome` /
  `ThrowRuntimeExAndDefault`（只有 `V1..V8`）。前代用的是 jni-rs git master。要用新 API 就得像前代那样
  走 git 依赖；否则用 0.21.1 + `JNIVersion::V8`（JNI 1.8 表，兼容 21 运行时）。
- cargo 走 `rsproxy.cn` 镜像（`C:\Users\winxp\.cargo\config`）。
- **github.com 被连接重置**，但 codeload.github.com 可达。
