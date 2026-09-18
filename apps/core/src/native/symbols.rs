//! The JNI native entry points we hook, and how their addresses are resolved.
//!
//! V1 showed the cost of guessing these names: the read path is
//! `Java_java_io_RandomAccessFile_readBytes0` (with a trailing `0`), and
//! `RandomAccessFile` has **no** `close0` at all — closing goes through
//! `Java_java_io_FileDescriptor_close0`, whose receiver is the `FileDescriptor`
//! itself rather than the stream. Two plausible-looking names, both wrong, both
//! costing a JVM launch to discover.
//!
//! So the names live here, in one table, and there is a test that checks them
//! against the real `java.dll` export table when a JDK is available.

/// One hooked native method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeSymbol {
    /// The exported symbol name in `java.dll`.
    pub symbol: &'static str,
    /// Which family it belongs to, used for install ordering and stats.
    pub family: SymbolFamily,
    /// A short label for logs and hook-hit counters.
    pub label: &'static str,
}

/// Which concern a symbol serves.
///
/// The ordering matters: `HollowRead` and `FileAttributes` come from
/// `java.dll`, which the JVM loads during creation; `NioRead` comes from
/// `nio.dll`, which is loaded lazily and so must be forced before its hooks
/// can be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolFamily {
    /// `RandomAccessFile` reads, which is what `ZipFile` uses.
    HollowRead,
    /// `java.io.File` attribute queries, which `File.length()` goes through.
    FileAttributes,
    /// `java.nio.channels.FileChannel` reads, which `Files.*` uses.
    NioRead,
    /// `sun.nio.fs` file opening and sizing, which is how `Files.*` obtains the
    /// handle those reads then use.
    NioOpen,
    /// Virtual-mapped native libraries loaded through `System.load`.
    NativeLoad,
    /// JVM-internal library loading, hooked last because it needs `jvm.dll`.
    JvmLibrary,
}

impl SymbolFamily {
    /// Which module the symbol is exported from.
    #[must_use]
    pub fn module(self) -> &'static str {
        match self {
            // `RandomAccessFile` and `WinNTFileSystem` both live in java.dll.
            Self::HollowRead | Self::FileAttributes => "java.dll",
            Self::NioRead | Self::NioOpen => "nio.dll",
            Self::NativeLoad | Self::JvmLibrary => "java.dll",
        }
    }
}

/// The symbols v4 installs today.
///
/// Archive, stream, and descriptor operations used to read virtual artifacts.
///
/// `ZipFile` uses `RandomAccessFile`, while libraries such as Guava use
/// `FileInputStream` directly. Both must acquire synthetic handles so a virtual
/// path never falls through to the host filesystem.
pub const HOLLOW_READ_SYMBOLS: &[NativeSymbol] = &[
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_open0",
        family: SymbolFamily::HollowRead,
        label: "raf.open0",
    },
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_read0",
        family: SymbolFamily::HollowRead,
        label: "raf.read0",
    },
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_readBytes0",
        family: SymbolFamily::HollowRead,
        label: "raf.readBytes0",
    },
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_length0",
        family: SymbolFamily::HollowRead,
        label: "raf.length0",
    },
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_seek0",
        family: SymbolFamily::HollowRead,
        label: "raf.seek0",
    },
    NativeSymbol {
        symbol: "Java_java_io_RandomAccessFile_getFilePointer",
        family: SymbolFamily::HollowRead,
        label: "raf.getFilePointer",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_open0",
        family: SymbolFamily::HollowRead,
        label: "fis.open0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_read0",
        family: SymbolFamily::HollowRead,
        label: "fis.read0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_readBytes",
        family: SymbolFamily::HollowRead,
        label: "fis.readBytes",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_length0",
        family: SymbolFamily::HollowRead,
        label: "fis.length0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_position0",
        family: SymbolFamily::HollowRead,
        label: "fis.position0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_skip0",
        family: SymbolFamily::HollowRead,
        label: "fis.skip0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_available0",
        family: SymbolFamily::HollowRead,
        label: "fis.available0",
    },
    NativeSymbol {
        symbol: "Java_java_io_FileInputStream_isRegularFile0",
        family: SymbolFamily::HollowRead,
        label: "fis.isRegularFile0",
    },
    // Not a RandomAccessFile native: closing goes through FileDescriptor, and
    // the receiver is the FileDescriptor rather than the stream.
    NativeSymbol {
        symbol: "Java_java_io_FileDescriptor_close0",
        family: SymbolFamily::HollowRead,
        label: "fd.close0",
    },
];

/// `java.io.File` attribute queries.
///
/// `File.length()` and `File.isFile()` must report the *virtual* length and
/// existence, not the host filesystem's. `File.length()` is not merely cosmetic:
/// code that sizes a buffer from it would read zero bytes and then fail
/// confusingly far from the cause.
pub const FILE_ATTRIBUTE_SYMBOLS: &[NativeSymbol] = &[
    NativeSymbol {
        symbol: "Java_java_io_WinNTFileSystem_getLength0",
        family: SymbolFamily::FileAttributes,
        label: "fs.getLength0",
    },
    NativeSymbol {
        symbol: "Java_java_io_WinNTFileSystem_getBooleanAttributes0",
        family: SymbolFamily::FileAttributes,
        label: "fs.getBooleanAttributes0",
    },
];

/// `java.nio.channels.FileChannel` reads, exported from `nio.dll`.
///
/// `Files.readAllBytes` and `Files.size` do not go through
/// `RandomAccessFile` at all; they open a `FileChannel` and call these. That is
/// why the `ZipFile` hooks are not enough for them.
///
/// `nio.dll` is loaded lazily, so it must be forced with `LoadLibraryW` before
/// these can be resolved — unlike `java.dll`, which `jvm.dll` loads during
/// creation.
pub const NIO_READ_SYMBOLS: &[NativeSymbol] = &[
    NativeSymbol {
        symbol: "Java_sun_nio_ch_FileDispatcherImpl_size0",
        family: SymbolFamily::NioRead,
        label: "nio.size0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_ch_FileDispatcherImpl_read0",
        family: SymbolFamily::NioRead,
        label: "nio.read0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_ch_FileDispatcherImpl_pread0",
        family: SymbolFamily::NioRead,
        label: "nio.pread0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_ch_FileDispatcherImpl_seek0",
        family: SymbolFamily::NioRead,
        label: "nio.seek0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_ch_FileDispatcherImpl_close0",
        family: SymbolFamily::NioRead,
        label: "nio.close0",
    },
    // `Files.*` does not open through `FileDispatcherImpl` at all. It goes
    // through `sun.nio.fs.WindowsChannelFactory`, which calls
    // `WindowsNativeDispatcher.CreateFile0` to get a Win32 handle and *then*
    // wraps it in a `FileChannelImpl` that reads via `FileDispatcherImpl`.
    //
    // So the `FileDispatcherImpl` detours see a handle they have never been
    // told about — which is exactly why `Files.size` kept returning zero even
    // after those hooks were installed. `CreateFile0` is where the path is
    // visible, so it is where the association has to be recorded.
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_CreateFile0",
        family: SymbolFamily::NioOpen,
        label: "niofs.createFile0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileSizeEx",
        family: SymbolFamily::NioOpen,
        label: "niofs.getFileSizeEx",
    },
    // `Files.size` reaches this rather than `FileDispatcherImpl.size0`: the
    // nio filesystem layer asks for the whole `BY_HANDLE_FILE_INFORMATION`
    // struct, so the size has to be corrected inside the buffer the JDK reads.
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileInformationByHandle0",
        family: SymbolFamily::NioOpen,
        label: "niofs.getFileInfoByHandle0",
    },
    // What `Files.size` actually reaches. It carries the path as a native wide
    // string and a buffer for `WIN32_FILE_ATTRIBUTE_DATA`, so it needs no
    // handle mapping: the detour can correct the size in place.
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributesEx0",
        family: SymbolFamily::NioOpen,
        label: "niofs.getFileAttributesEx0",
    },
    // Path existence and `toRealPath()` traversal need direct metadata even when
    // no artifact file exists for the OS to stat.
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_GetFileAttributes0",
        family: SymbolFamily::NioOpen,
        label: "niofs.getFileAttributes0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_FindFirstFile0",
        family: SymbolFamily::NioOpen,
        label: "niofs.findFirstFile0",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_FindClose",
        family: SymbolFamily::NioOpen,
        label: "niofs.findClose",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_CloseHandle",
        family: SymbolFamily::NioOpen,
        label: "niofs.closeHandle",
    },
    NativeSymbol {
        symbol: "Java_sun_nio_fs_WindowsNativeDispatcher_GetFinalPathNameByHandle",
        family: SymbolFamily::NioOpen,
        label: "niofs.getFinalPathNameByHandle",
    },
];

/// Native-library loading entry points, both families.
///
/// JDK 21 added the `RawNativeLibraries` path behind `System.load`. Hooking
/// only `NativeLibraries_*` leaves a bypass through which a native library
/// would be loaded from disk and written out by the OS loader.
pub const NATIVE_LOAD_SYMBOLS: &[NativeSymbol] = &[
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_NativeLibraries_findBuiltinLib",
        family: SymbolFamily::NativeLoad,
        label: "nativelibs.findBuiltinLib",
    },
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_NativeLibraries_load",
        family: SymbolFamily::NativeLoad,
        label: "nativelibs.load",
    },
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_NativeLibraries_unload",
        family: SymbolFamily::NativeLoad,
        label: "nativelibs.unload",
    },
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_NativeLibrary_findEntry0",
        family: SymbolFamily::NativeLoad,
        label: "nativelib.findEntry0",
    },
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_RawNativeLibraries_load0",
        family: SymbolFamily::NativeLoad,
        label: "raw.load0",
    },
    NativeSymbol {
        symbol: "Java_jdk_internal_loader_RawNativeLibraries_unload0",
        family: SymbolFamily::NativeLoad,
        label: "raw.unload0",
    },
];

/// How many symbols the harness installs detours for.
///
/// Exposed so tests can assert "all of them" without hard-coding a number that
/// silently goes stale as coverage grows.
#[must_use]
pub fn hooked_symbol_count() -> usize {
    // Everything with an installed detour. The native-load family is included
    // because HotSpot detours are installed even on a VM that never calls them,
    // and the count is about what the harness set up, not what fires.
    HOLLOW_READ_SYMBOLS.len()
        + FILE_ATTRIBUTE_SYMBOLS.len()
        + NIO_READ_SYMBOLS.len()
        + NATIVE_LOAD_SYMBOLS.len()
}

/// Every symbol v4 knows how to hook.
#[must_use]
pub fn all_symbols() -> Vec<NativeSymbol> {
    HOLLOW_READ_SYMBOLS
        .iter()
        .chain(FILE_ATTRIBUTE_SYMBOLS.iter())
        .chain(NIO_READ_SYMBOLS.iter())
        .chain(NATIVE_LOAD_SYMBOLS.iter())
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn symbol_names_are_unique() {
        let symbols = all_symbols();
        let unique: HashSet<&str> = symbols.iter().map(|s| s.symbol).collect();
        assert_eq!(unique.len(), symbols.len(), "duplicate symbol in the table");
    }

    #[test]
    fn labels_are_unique_so_stats_do_not_collide() {
        let symbols = all_symbols();
        let unique: HashSet<&str> = symbols.iter().map(|s| s.label).collect();
        assert_eq!(unique.len(), symbols.len(), "duplicate hook label");
    }

    #[test]
    fn every_symbol_is_a_jni_export_name() {
        for symbol in all_symbols() {
            assert!(
                symbol.symbol.starts_with("Java_"),
                "{} is not a JNI export name",
                symbol.symbol
            );
        }
    }

    /// The specific naming traps V1 paid for. If someone "tidies" these, the
    /// hooks silently stop firing and jars read as empty.
    #[test]
    fn the_read_path_uses_the_name_the_jdk_actually_exports() {
        assert!(
            HOLLOW_READ_SYMBOLS
                .iter()
                .any(|s| s.symbol == "Java_java_io_RandomAccessFile_readBytes0"),
            "the export is readBytes0, not readBytes"
        );
        assert!(
            HOLLOW_READ_SYMBOLS
                .iter()
                .any(|s| s.symbol == "Java_java_io_FileDescriptor_close0"),
            "closing goes through FileDescriptor, not RandomAccessFile"
        );
        assert!(
            !HOLLOW_READ_SYMBOLS
                .iter()
                .any(|s| s.symbol.contains("RandomAccessFile_close0")),
            "RandomAccessFile has no close0 export"
        );
        for symbol in [
            "Java_java_io_FileInputStream_open0",
            "Java_java_io_FileInputStream_readBytes",
            "Java_java_io_FileInputStream_isRegularFile0",
        ] {
            assert!(
                HOLLOW_READ_SYMBOLS
                    .iter()
                    .any(|entry| entry.symbol == symbol),
                "FileInputStream export {symbol} must remain hooked"
            );
        }
    }

    #[test]
    fn the_nio_family_is_exported_from_nio_dll() {
        for symbol in NIO_READ_SYMBOLS {
            assert_eq!(
                symbol.family.module(),
                "nio.dll",
                "{} must be resolved from nio.dll",
                symbol.symbol
            );
        }
    }

    #[test]
    fn file_attribute_symbols_come_from_java_dll() {
        for symbol in FILE_ATTRIBUTE_SYMBOLS {
            assert_eq!(symbol.family.module(), "java.dll");
        }
    }

    #[test]
    fn both_native_load_families_are_covered() {
        let names: Vec<&str> = NATIVE_LOAD_SYMBOLS.iter().map(|s| s.symbol).collect();
        assert!(names.contains(&"Java_jdk_internal_loader_RawNativeLibraries_load0"));
        assert!(names.contains(&"Java_jdk_internal_loader_NativeLibraries_load"));
    }
}
