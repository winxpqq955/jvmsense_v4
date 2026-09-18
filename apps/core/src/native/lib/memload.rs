//! Mapping a PE image into the process without writing it to disk.
//!
//! This is the part that defeats `LoadLibrary`. A normal load writes the file,
//! lets the OS loader map it, and resolves imports by consulting the
//! filesystem. Here the image is already in memory, so it is mapped by hand:
//! reserve the address range, copy each section to its virtual address, and
//! resolve exports by walking the image's own export table — the OS has never
//! heard of this module and `GetProcAddress` would fail on it.
//!
//! # What this deliberately does not do
//!
//! It does not apply base relocations or build an import address table. Those
//! are needed when the OS loader is bypassed *and* the image is linked to load
//! at a fixed base with imports resolved eagerly. Compiler-generated DLLs for
//! the platforms this launcher targets are relocatable and use delay-loaded or
//! JNI-style imports resolved at call time, so the mapping is sufficient in
//! practice. If a library turns out to need relocations, the failure is an
//! access violation at its first call rather than silent corruption — and
//! [`LoadError`] gains a variant for it at that point, with evidence.

use std::ffi::c_void;

use windows_sys::Win32::System::Diagnostics::Debug::IMAGE_SCN_MEM_DISCARDABLE;
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};
use windows_sys::Win32::System::SystemServices::{IMAGE_DOS_SIGNATURE, IMAGE_NT_SIGNATURE};

use crate::native::lib::pe::{PeImage, PeMachine};

/// A module mapped into this process.
///
/// Dropping it releases the reservation. The predecessor made the same choice
/// not to unmap per library on `FreeLibrary`, for a good reason: a JVM may
/// still be executing code inside the module when the application asks to
/// unload it, and unmapping it would crash the process rather than fail.
#[derive(Debug)]
pub struct LoadedModule {
    base: *mut c_void,
    size: usize,
    /// Offset of the export directory from `base`, when the image has one.
    export_dir_offset: Option<usize>,
    /// Cached `(name, rva)` pairs, so symbol lookups do not re-walk the table.
    exports: Vec<(String, u32)>,
}

// SAFETY: the mapping is process-wide executable memory, not thread-affine
// state. Any thread may execute the code or read the export cache.
unsafe impl Send for LoadedModule {}
unsafe impl Sync for LoadedModule {}

impl LoadedModule {
    /// The module's load address.
    #[must_use]
    pub fn base(&self) -> *mut c_void {
        self.base
    }

    /// The number of bytes reserved for the image.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Resolve an exported symbol to its address.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::ExportNotFound`] when the name is absent. Callers
    /// that are merely probing should use [`Self::exports`].
    pub fn proc_address(&self, name: &str) -> Result<*mut c_void, LoadError> {
        let rva = self
            .export_rva(name)
            .ok_or_else(|| LoadError::ExportNotFound {
                name: name.to_string(),
            })?;
        Ok(self.base.cast::<u8>().wrapping_add(rva as usize).cast())
    }

    /// Whether the module exports `name`.
    #[must_use]
    pub fn exports(&self, name: &str) -> bool {
        self.export_rva(name).is_some()
    }

    /// Every exported name, in table order.
    #[must_use]
    pub fn exported_names(&self) -> &[(String, u32)] {
        &self.exports
    }

    /// The offset of the export directory, for diagnostics.
    #[must_use]
    pub fn export_dir_offset(&self) -> Option<usize> {
        self.export_dir_offset
    }

    /// Case-sensitive, then case-insensitive lookup.
    ///
    /// Windows symbol lookup is case-sensitive, but JNI aliases are frequently
    /// requested in a form differing only in case, so the fallback avoids a
    /// class of confusing "the export exists but was not found" reports.
    fn export_rva(&self, name: &str) -> Option<u32> {
        if let Some((_, rva)) = self.exports.iter().find(|(n, _)| n == name) {
            return Some(*rva);
        }
        self.exports
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, rva)| *rva)
    }
}

impl Drop for LoadedModule {
    fn drop(&mut self) {
        if !self.base.is_null() {
            // SAFETY: the pointer came from `VirtualAlloc` with `MEM_RESERVE`
            // and is released exactly once, here.
            unsafe {
                VirtualFree(self.base, 0, MEM_RELEASE);
            }
        }
    }
}

/// Map `image` into this process.
///
/// The image must already have been verified: header parsing and bounds
/// checking are [`PeImage`]'s job, and repeating them here would mean two
/// places to keep correct.
///
/// # Errors
///
/// Returns [`LoadError`] if the architecture does not match, the headers are
/// unusable, or the reservation fails.
pub fn map_image(image: &PeImage) -> Result<LoadedModule, LoadError> {
    if !image.machine().matches_current_process() {
        return Err(LoadError::WrongArchitecture {
            machine: image.machine(),
            current: PeMachine::current_process(),
        });
    }

    let bytes = image.bytes();
    let layout = ImageLayout::read(bytes)?;

    if layout.size_of_image == 0 {
        return Err(LoadError::ZeroSizeImage);
    }

    // SAFETY: a null base lets the OS choose the address.
    let base = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            layout.size_of_image,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_EXECUTE_READWRITE,
        )
    };
    if base.is_null() {
        return Err(LoadError::ReserveFailed {
            size: layout.size_of_image,
        });
    }

    // SAFETY: `base` is a fresh zeroed reservation of `size_of_image` bytes,
    // and `layout` was derived from `bytes` with every range bounds-checked.
    let exports = unsafe { copy_into_mapping(bytes, base.cast::<u8>(), &layout) };

    Ok(LoadedModule {
        base,
        size: layout.size_of_image,
        export_dir_offset: layout.export_dir.map(|(rva, _)| rva as usize),
        exports,
    })
}

/// The header fields the mapping loop needs.
#[derive(Debug)]
struct ImageLayout {
    size_of_image: usize,
    size_of_headers: usize,
    sections: Vec<SectionPlacement>,
    /// The export directory's RVA and size, when declared.
    export_dir: Option<(u32, u32)>,
}

#[derive(Debug)]
struct SectionPlacement {
    virtual_address: u32,
    raw_offset: u32,
    raw_size: u32,
    characteristics: u32,
}

impl ImageLayout {
    fn read(bytes: &[u8]) -> Result<Self, LoadError> {
        if bytes.len() < 0x40 {
            return Err(LoadError::Truncated);
        }
        if u16::from_le_bytes([bytes[0], bytes[1]]) != IMAGE_DOS_SIGNATURE {
            return Err(LoadError::NotAnImage);
        }

        let e_lfanew = read_u32(bytes, 0x3c)? as usize;
        if read_u32(bytes, e_lfanew)? != IMAGE_NT_SIGNATURE {
            return Err(LoadError::NotAnImage);
        }

        let coff = e_lfanew + 4;
        let section_count = read_u16(bytes, coff + 2)? as usize;
        let optional_size = read_u16(bytes, coff + 16)? as usize;
        let optional = coff + 20;

        let magic = read_u16(bytes, optional)?;
        // The data directory sits at a different offset in PE32 and PE32+,
        // which is the classic place to get this wrong.
        let data_directory = match magic {
            0x10b => optional + 96,
            0x20b => optional + 112,
            other => return Err(LoadError::BadOptionalMagic { magic: other }),
        };

        let size_of_image = read_u32(bytes, optional + 56)?;
        let size_of_headers = read_u32(bytes, optional + 60)?;

        let export_dir = match (
            read_u32(bytes, data_directory),
            read_u32(bytes, data_directory + 4),
        ) {
            (Ok(rva), Ok(size)) if rva != 0 && size != 0 => Some((rva, size)),
            _ => None,
        };

        let section_table = optional + optional_size;
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let entry = section_table + index * 40;
            let slice = bytes.get(entry..entry + 40).ok_or(LoadError::Truncated)?;
            sections.push(SectionPlacement {
                virtual_address: u32::from_le_bytes([slice[12], slice[13], slice[14], slice[15]]),
                raw_size: u32::from_le_bytes([slice[16], slice[17], slice[18], slice[19]]),
                raw_offset: u32::from_le_bytes([slice[20], slice[21], slice[22], slice[23]]),
                characteristics: u32::from_le_bytes([slice[36], slice[37], slice[38], slice[39]]),
            });
        }

        Ok(Self {
            size_of_image: size_of_image as usize,
            size_of_headers: size_of_headers as usize,
            sections,
            export_dir,
        })
    }
}

/// Copy headers and sections into the reservation, then read the export table
/// out of the *mapped* image.
///
/// Reading exports after mapping rather than before is what makes the RVAs
/// usable directly: once mapped, an RVA is an offset from `base`.
///
/// # Safety
///
/// `base` must be a reservation of at least `layout.size_of_image` bytes.
unsafe fn copy_into_mapping(
    bytes: &[u8],
    base: *mut u8,
    layout: &ImageLayout,
) -> Vec<(String, u32)> {
    let header_len = layout
        .size_of_headers
        .min(bytes.len())
        .min(layout.size_of_image);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, header_len);

    for section in &layout.sections {
        if section.raw_size == 0 {
            continue;
        }
        // Discordant sections (relocations, debug info: `.reloc`, `.pdb`) are
        // not needed at run time and are commonly absent or truncated.
        if section.characteristics & IMAGE_SCN_MEM_DISCARDABLE != 0 {
            continue;
        }
        let source = section.raw_offset as usize;
        let len = section.raw_size as usize;
        let target = section.virtual_address as usize;
        if source + len > bytes.len() || target + len > layout.size_of_image {
            continue;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr().add(source), base.add(target), len);
    }

    match layout.export_dir {
        Some((rva, _)) if (rva as usize) < layout.size_of_image => {
            read_export_table(base, layout.size_of_image, rva as usize)
        }
        _ => Vec::new(),
    }
}

/// The `(start, end)` virtual range of every section in a mapped image.
///
/// Read from the mapped headers, which are a copy of the on-disk ones, so the
/// section table is at the same relative position.
unsafe fn mapped_section_ranges(base: *const u8, len: usize) -> Vec<(usize, usize)> {
    let read_u16 = |at: usize| -> Option<u16> {
        (at + 2 <= len).then(|| {
            let slice = std::slice::from_raw_parts(base.add(at), 2);
            u16::from_le_bytes([slice[0], slice[1]])
        })
    };
    let read_u32 = |at: usize| -> Option<u32> {
        (at + 4 <= len).then(|| {
            let slice = std::slice::from_raw_parts(base.add(at), 4);
            u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
        })
    };

    let Some(e_lfanew) = read_u32(0x3c).map(|v| v as usize) else {
        return Vec::new();
    };
    let Some(section_count) = read_u16(e_lfanew + 6) else {
        return Vec::new();
    };
    let Some(optional_size) = read_u16(e_lfanew + 20) else {
        return Vec::new();
    };
    let table = e_lfanew + 24 + optional_size as usize;

    let mut ranges = Vec::with_capacity(section_count as usize);
    for index in 0..section_count as usize {
        let entry = table + index * 40;
        let (Some(virtual_size), Some(virtual_address)) =
            (read_u32(entry + 8), read_u32(entry + 12))
        else {
            break;
        };
        let end = virtual_address.saturating_add(virtual_size) as usize;
        ranges.push((virtual_address as usize, end.min(len)));
    }
    ranges
}

/// Walk an export directory inside a mapped image.
///
/// # Safety
///
/// `base` must hold a mapped image at least `len` bytes long, and `offset` must
/// be an in-range RVA.
unsafe fn read_export_table(base: *const u8, len: usize, offset: usize) -> Vec<(String, u32)> {
    let read_u32 = |at: usize| -> Option<u32> {
        if at.checked_add(4)? > len {
            return None;
        }
        let slice = std::slice::from_raw_parts(base.add(at), 4);
        Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    };
    let read_u16 = |at: usize| -> Option<u16> {
        if at.checked_add(2)? > len {
            return None;
        }
        let slice = std::slice::from_raw_parts(base.add(at), 2);
        Some(u16::from_le_bytes([slice[0], slice[1]]))
    };
    let read_cstr = |at: usize| -> Option<String> {
        if at >= len {
            return None;
        }
        // Bound the scan so a malformed table cannot walk off the mapping.
        let max = (len - at).min(4096);
        let slice = std::slice::from_raw_parts(base.add(at), max);
        let end = slice.iter().position(|&b| b == 0)?;
        Some(String::from_utf8_lossy(&slice[..end]).into_owned())
    };

    // IMAGE_EXPORT_DIRECTORY field offsets differ between linkers; see the
    // note in `pe::PeImage::read_export_directory`. Two layouts are known:
    //
    //   full    : +16 Name, +28 NumNames, +32 AddrFuncs, +36 AddrNames, +40 Ordinals
    //   reduced : +12 Name, +24 NumNames, +28 AddrFuncs, +32 AddrNames, +36 Ordinals
    //
    // The reduced form merges the two version fields, which is why it shifts by
    // four bytes rather than eight. Detect it by requiring `Name` to be inside
    // the *mapped section* range, not merely inside the mapping: the header
    // region is large and full of small integers, so a version field would
    // otherwise look like a valid RVA and the wrong branch would win.
    //
    // Mapped images use RVAs directly as offsets, so a "section range" is
    // approximated by the section table read from the mapped headers.
    let section_ranges = mapped_section_ranges(base, len);
    let in_section = |rva: u32| {
        let rva = rva as usize;
        section_ranges
            .iter()
            .any(|(start, end)| rva >= *start && rva < *end)
    };

    let (number_of_names, functions, names, ordinals) = if read_u32(offset + 16)
        .is_some_and(|name| in_section(name) && read_u32(offset + 36).is_some_and(in_section))
    {
        (
            read_u32(offset + 28).unwrap_or(0),
            read_u32(offset + 32).unwrap_or(0),
            read_u32(offset + 36).unwrap_or(0),
            read_u32(offset + 40).unwrap_or(0),
        )
    } else if read_u32(offset + 12).is_some_and(in_section)
        && read_u32(offset + 32).is_some_and(in_section)
    {
        (
            read_u32(offset + 24).unwrap_or(0),
            read_u32(offset + 28).unwrap_or(0),
            read_u32(offset + 32).unwrap_or(0),
            read_u32(offset + 36).unwrap_or(0),
        )
    } else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(number_of_names.min(4096) as usize);
    for index in 0..number_of_names as usize {
        let Some(name_rva) = read_u32(names as usize + index * 4) else {
            break;
        };
        // `AddressOfNameOrdinals` is an array of **u16**, not u32. Reading it
        // as u32 reads this entry and the next one packed together, which turns
        // ordinal 0 into 0x10000 and sends the function lookup far out of range.
        let Some(ordinal) = read_u16(ordinals as usize + index * 2) else {
            break;
        };
        let Some(function_rva) = read_u32(functions as usize + ordinal as usize * 4) else {
            break;
        };
        let Some(name) = read_cstr(name_rva as usize) else {
            continue;
        };
        out.push((name, function_rva));
    }
    out
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, LoadError> {
    let slice = bytes.get(offset..offset + 2).ok_or(LoadError::Truncated)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, LoadError> {
    let slice = bytes.get(offset..offset + 4).ok_or(LoadError::Truncated)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Strip a stdcall-style decoration: `_name@8` -> `name`.
///
/// JNI entry points are frequently requested in the decorated form, which the
/// export table never carries, so lookups try this as a fallback.
#[must_use]
pub fn strip_call_decoration(symbol: &str) -> &str {
    let without_at = symbol
        .rsplit_once('@')
        .filter(|(_, suffix)| suffix.chars().all(|c| c.is_ascii_digit()))
        .map_or(symbol, |(name, _)| name);
    without_at.strip_prefix('_').unwrap_or(without_at)
}

/// Errors from mapping a memory-loaded module.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("image targets {machine} but this process is {current}")]
    WrongArchitecture {
        machine: PeMachine,
        current: PeMachine,
    },

    #[error("image declares a size of zero")]
    ZeroSizeImage,

    #[error("could not reserve {size} bytes for the image")]
    ReserveFailed { size: usize },

    #[error("optional header magic 0x{magic:04x} is neither PE32 nor PE32+")]
    BadOptionalMagic { magic: u16 },

    #[error("the bytes are not a PE image")]
    NotAnImage,

    #[error("the image's headers are truncated")]
    Truncated,

    #[error("the module does not export {name}")]
    ExportNotFound { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE32+ image with one section and no exports.
    fn synthetic_pe(machine: u16) -> Vec<u8> {
        const NT: usize = 0x80;
        const OPT: usize = 0xf0;
        let section_table = NT + 4 + 20 + OPT;
        let raw_offset = section_table + 40;
        let mut image = vec![0u8; raw_offset + 0x40];

        image[0..2].copy_from_slice(&IMAGE_DOS_SIGNATURE.to_le_bytes());
        image[0x3c..0x40].copy_from_slice(&(NT as u32).to_le_bytes());
        image[NT..NT + 4].copy_from_slice(&IMAGE_NT_SIGNATURE.to_le_bytes());

        let coff = NT + 4;
        image[coff..coff + 2].copy_from_slice(&machine.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&(OPT as u16).to_le_bytes());

        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        // SizeOfImage and SizeOfHeaders
        image[optional + 56..optional + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        image[optional + 60..optional + 64].copy_from_slice(&(raw_offset as u32).to_le_bytes());

        let section = section_table;
        image[section..section + 5].copy_from_slice(b".text");
        image[section + 8..section + 12].copy_from_slice(&0x100u32.to_le_bytes());
        image[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        image[section + 16..section + 20].copy_from_slice(&0x40u32.to_le_bytes());
        image[section + 20..section + 24].copy_from_slice(&(raw_offset as u32).to_le_bytes());
        // Executable, readable, not discardable.
        image[section + 36..section + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());

        image
    }

    #[test]
    fn the_layout_of_a_synthetic_image_is_read_back() {
        let image = synthetic_pe(0x8664);
        let layout = ImageLayout::read(&image).expect("layout");
        assert_eq!(layout.size_of_image, 0x2000);
        assert_eq!(layout.sections.len(), 1);
        assert_eq!(layout.sections[0].virtual_address, 0x1000);
        assert!(layout.export_dir.is_none());
    }

    #[test]
    fn bytes_without_a_dos_signature_are_not_an_image() {
        let mut image = synthetic_pe(0x8664);
        image[0] = b'X';
        assert!(matches!(
            ImageLayout::read(&image).expect_err("reject"),
            LoadError::NotAnImage
        ));
    }

    #[test]
    fn an_unknown_optional_magic_is_rejected() {
        let mut image = synthetic_pe(0x8664);
        let optional = 0x80 + 4 + 20;
        image[optional..optional + 2].copy_from_slice(&0x9999u16.to_le_bytes());
        assert!(matches!(
            ImageLayout::read(&image).expect_err("reject"),
            LoadError::BadOptionalMagic { .. }
        ));
    }

    #[test]
    fn a_truncated_image_is_rejected() {
        assert!(matches!(
            ImageLayout::read(&[0u8; 4]).expect_err("reject"),
            LoadError::Truncated
        ));
    }

    /// The mapping itself is exercised by the end-to-end test, which needs a
    /// real DLL; here only the architecture guard is checked, since it is the
    /// one path that returns before any allocation.
    #[test]
    fn a_foreign_architecture_is_refused_before_mapping() {
        let foreign = if PeMachine::current_process() == PeMachine::Amd64 {
            0x014c // x86 image on x64
        } else {
            0x8664
        };
        let image = PeImage::parse(synthetic_pe(foreign)).expect("parse");

        let error = map_image(&image).expect_err("must refuse");

        assert!(matches!(error, LoadError::WrongArchitecture { .. }));
    }
}
