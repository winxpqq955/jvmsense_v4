//! Parsing and validating Portable Executable images held in memory.
//!
//! The predecessor hand-rolled this inside its registry module. Keeping it
//! separate matters because it is the part that decides whether an untrusted
//! sequence of bytes is safe to hand to a memory loader: every offset and
//! length here comes from the file itself, so each one has to be bounds-checked
//! rather than trusted.
//!
//! Nothing in this module touches the filesystem or Windows, which is what
//! lets the whole thing be unit-tested against synthetic images.

use std::fmt;

/// The machine a PE image targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeMachine {
    I386,
    Amd64,
    Arm64,
    /// A machine value this build does not recognize.
    Other(u16),
}

impl PeMachine {
    /// The machine this process is running as.
    #[must_use]
    pub fn current_process() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self::Amd64
        }
        #[cfg(target_arch = "x86")]
        {
            Self::I386
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self::Arm64
        }
    }

    fn from_raw(value: u16) -> Self {
        match value {
            0x014c => Self::I386,
            0x8664 => Self::Amd64,
            0xaa64 => Self::Arm64,
            other => Self::Other(other),
        }
    }

    /// Whether this machine matches the running process.
    #[must_use]
    pub fn matches_current_process(self) -> bool {
        self == Self::current_process()
    }
}

impl fmt::Display for PeMachine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I386 => f.write_str("x86"),
            Self::Amd64 => f.write_str("x86-64"),
            Self::Arm64 => f.write_str("arm64"),
            Self::Other(value) => write!(f, "machine 0x{value:04x}"),
        }
    }
}

/// A validated PE image.
///
/// Constructing one proves the headers are structurally sound: the DOS and NT
/// signatures are present, the section table lies inside the file, and every
/// section's file range lies inside the file. It does *not* prove the code is
/// safe to run — nothing can — but it does mean the memory loader will not be
/// handed offsets that walk off the end of the buffer.
#[derive(Debug, Clone)]
pub struct PeImage {
    bytes: Vec<u8>,
    machine: PeMachine,
    /// `(name, virtual_address, virtual_size, raw_offset, raw_size)` per section.
    sections: Vec<Section>,
    /// The export directory's RVA and size, when present.
    export_table: Option<(u32, u32)>,
    /// The image's preferred base address.
    image_base: u64,
}

#[derive(Debug, Clone)]
struct Section {
    name: String,
    virtual_address: u32,
    virtual_size: u32,
    raw_offset: u32,
    raw_size: u32,
}

impl PeImage {
    /// Parse and validate an image.
    ///
    /// # Errors
    ///
    /// Returns [`PeError`] for any structural problem. Every failure mode is an
    /// error rather than a default, because a partially-understood image is
    /// exactly the input a memory loader must not be given.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, PeError> {
        let header = Header::read(&bytes)?;

        let mut sections = Vec::with_capacity(header.section_count as usize);
        for index in 0..header.section_count {
            let section = Section::read(&bytes, &header, index)?;
            // A section whose raw range escapes the file would make every
            // later read from it a potential out-of-bounds access.
            let end = u64::from(section.raw_offset) + u64::from(section.raw_size);
            if end > bytes.len() as u64 {
                return Err(PeError::SectionOutOfRange {
                    name: section.name,
                    offset: section.raw_offset,
                    size: section.raw_size,
                    file_len: bytes.len(),
                });
            }
            sections.push(section);
        }

        Ok(Self {
            bytes,
            machine: header.machine,
            sections,
            export_table: header.export_table,
            image_base: header.image_base,
        })
    }

    /// The machine this image targets.
    #[must_use]
    pub fn machine(&self) -> PeMachine {
        self.machine
    }

    /// The raw image bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The image's length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when the image is empty, which a valid PE never is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The export directory's RVA and size, for diagnostics.
    #[must_use]
    pub fn export_table_rva(&self) -> Option<(u32, u32)> {
        self.export_table
    }

    /// The preferred load address.
    #[must_use]
    pub fn image_base(&self) -> u64 {
        self.image_base
    }

    /// Section names, in header order.
    pub fn section_names(&self) -> impl Iterator<Item = &str> {
        self.sections.iter().map(|s| s.name.as_str())
    }

    /// Translate a relative virtual address to a file offset.
    ///
    /// Follows the loader's own rule: a section covers
    /// `max(virtual_size, raw_size)` bytes, so an RVA inside the slack of a
    /// section with a larger raw size still resolves.
    #[must_use]
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        for section in &self.sections {
            let start = section.virtual_address;
            let span = section.virtual_size.max(section.raw_size);
            if rva >= start && rva < start.saturating_add(span) {
                let delta = rva - start;
                let offset = section.raw_offset.saturating_add(delta);
                if (offset as usize) < self.bytes.len() {
                    return Some(offset as usize);
                }
                return None;
            }
        }
        // An RVA in the header region maps to itself, but only for RVAs that
        // actually fall inside the headers. Returning a match for *any* small
        // RVA is what made the reduced export-directory layout undetectable:
        // the version field `1` "mapped" to offset 1, so the wrong layout
        // branch won and read `AddressOfNames` out of the middle of a
        // neighbouring structure.
        let header_len = self
            .sections
            .iter()
            .map(|section| section.raw_offset)
            .filter(|offset| *offset > 0)
            .min()
            .unwrap_or(self.bytes.len() as u32);
        if rva < header_len {
            return Some(rva as usize);
        }
        None
    }

    /// Whether an RVA falls inside a real section's virtual range.
    ///
    /// Distinct from [`Self::rva_to_offset`], which also resolves header RVAs.
    /// Layout detection must use this stricter form: the header region is large
    /// (it spans from 0 up to the first section's raw offset) and contains many
    /// small integers, so a version field like `1` would otherwise look like a
    /// valid header RVA and the wrong layout branch would win.
    fn rva_in_section(&self, rva: u32) -> bool {
        self.sections.iter().any(|section| {
            let span = section.virtual_size.max(section.raw_size);
            rva >= section.virtual_address && rva < section.virtual_address.saturating_add(span)
        })
    }

    /// Read the export directory's field RVAs, tolerating both known layouts.
    ///
    /// Returns `(number_of_names, names, ordinals, functions)`.
    fn read_export_directory(&self, table: usize) -> Result<(u32, u32, u32, u32), PeError> {
        let read = |offset: usize| -> Result<u32, PeError> {
            let end = offset
                .checked_add(4)
                .ok_or(PeError::ExportTableNotMapped { rva: table as u32 })?;
            let slice = self
                .bytes
                .get(offset..end)
                .ok_or(PeError::ExportTableNotMapped { rva: table as u32 })?;
            Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
        };

        // The discriminator is `Name`. Under the full layout it is an RVA into
        // a mapped section; under a reduced one the same slot holds a small
        // integer (a version field) that will not map.
        //
        // Checking `AddressOfNames` instead does not work: in a reduced layout
        // that slot holds `AddressOfNameOrdinals`, which *is* a mappable RVA,
        // so the wrong branch would win.
        let standard_name = read(table + 16)?;
        if self.rva_in_section(standard_name) {
            let count = read(table + 28)?;
            let functions = read(table + 32)?;
            let names = read(table + 36)?;
            let ordinals = read(table + 40)?;
            if self.rva_in_section(names) {
                return Ok((count, names, ordinals, functions));
            }
        }

        // Reduced layout, measured against a Rust `cdylib` built by LLVM's
        // linker. Its directory is nine u32 fields rather than ten:
        //
        //   +0  Characteristics   (0)
        //   +4  TimeDateStamp     (0xffffffff)
        //   +8  Version           (one combined field, not Major+Minor)
        //   +12 Name
        //   +16 Base
        //   +20 NumberOfFunctions
        //   +24 NumberOfNames
        //   +28 AddressOfFunctions
        //   +32 AddressOfNames
        //   +36 AddressOfNameOrdinals
        //
        // The two version fields are merged, which is why the shift is four
        // bytes rather than eight. Reading from the winnt.h offsets on such an
        // image yields `AddressOfNames` = 0x10000 (the version field of the
        // next section), which is what the first attempt at this produced.
        let name = read(table + 12)?;
        if !self.rva_in_section(name) {
            return Err(PeError::ExportTableNotMapped { rva: name });
        }
        let count = read(table + 24)?;
        let functions = read(table + 28)?;
        let names = read(table + 32)?;
        let ordinals = read(table + 36)?;
        if !self.rva_in_section(names) {
            return Err(PeError::ExportTableNotMapped { rva: names });
        }
        Ok((count, names, ordinals, functions))
    }

    /// The names this image exports.
    ///
    /// Used to answer `GetProcAddress` for a memory-loaded module without
    /// asking the OS, and to log which symbols a library offers.
    ///
    /// # Errors
    ///
    /// Returns [`PeError`] if an export table is present but malformed. Absence
    /// of a table is not an error: many DLLs export nothing by name.
    pub fn exported_names(&self) -> Result<Vec<String>, PeError> {
        let Some((rva, size)) = self.export_table else {
            return Ok(Vec::new());
        };
        if size == 0 {
            return Ok(Vec::new());
        }
        let table = self
            .rva_to_offset(rva)
            .ok_or(PeError::ExportTableNotMapped { rva })?;

        let read_u32 = |offset: usize| -> Result<u32, PeError> {
            let end = offset
                .checked_add(4)
                .ok_or(PeError::ExportTableNotMapped { rva })?;
            let slice = self
                .bytes
                .get(offset..end)
                .ok_or(PeError::ExportTableNotMapped { rva })?;
            Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
        };

        // IMAGE_EXPORT_DIRECTORY per winnt.h:
        //   +0 Characteristics, +4 TimeDateStamp, +8 MajorVersion,
        //   +12 MinorVersion, +16 Name, +20 Base, +24 NumberOfFunctions,
        //   +28 NumberOfNames, +32 AddressOfFunctions, +36 AddressOfNames,
        //   +40 AddressOfNameOrdinals.
        //
        // Except that some linkers — LLVM's, which is what builds a Rust
        // `cdylib` — emit the directory with `Characteristics` omitted, so the
        // RVA in the data directory points straight at `TimeDateStamp` and
        // every field sits four bytes earlier than the struct says.
        //
        // Both layouts are in the wild, and they are not distinguishable by
        // the RVA alone. They are distinguishable by their contents: the
        // `Name` RVA must be inside a mapped section, and it is a small
        // integer (the version fields) under the wrong reading. So try the
        // standard layout and fall back when it does not validate.
        let (number_of_names, names_rva, ordinals_rva, functions_rva) =
            self.read_export_directory(table)?;
        let names_offset = self
            .rva_to_offset(names_rva)
            .ok_or(PeError::ExportTableNotMapped { rva: names_rva })?;
        let _ = (ordinals_rva, functions_rva);

        let mut names = Vec::with_capacity(number_of_names.min(4096) as usize);
        for index in 0..number_of_names {
            let entry = names_offset + (index as usize) * 4;
            let name_rva = read_u32(entry)?;
            let name_offset = self
                .rva_to_offset(name_rva)
                .ok_or(PeError::ExportTableNotMapped { rva: name_rva })?;
            let rest = self
                .bytes
                .get(name_offset..)
                .ok_or(PeError::ExportTableNotMapped { rva: name_rva })?;
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or(PeError::UnterminatedExportName { rva: name_rva })?;
            names.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        }
        Ok(names)
    }
}

/// The fields this module needs out of the PE headers.
struct Header {
    machine: PeMachine,
    section_count: u16,
    section_table_offset: usize,
    export_table: Option<(u32, u32)>,
    image_base: u64,
}

impl Header {
    fn read(bytes: &[u8]) -> Result<Self, PeError> {
        // MZ
        if bytes.len() < 0x40 {
            return Err(PeError::TooShort { len: bytes.len() });
        }
        if &bytes[0..2] != b"MZ" {
            return Err(PeError::MissingDosSignature);
        }

        let e_lfanew = u32::from_le_bytes([bytes[0x3c], bytes[0x3d], bytes[0x3e], bytes[0x3f]]);
        let nt = e_lfanew as usize;
        let signature_end = nt
            .checked_add(4)
            .ok_or(PeError::BadNtOffset { offset: nt })?;
        let signature = bytes
            .get(nt..signature_end)
            .ok_or(PeError::BadNtOffset { offset: nt })?;
        if signature != b"PE\0\0" {
            return Err(PeError::MissingNtSignature);
        }

        let coff = signature_end;
        let coff_end = coff
            .checked_add(20)
            .ok_or(PeError::BadNtOffset { offset: nt })?;
        let coff_bytes = bytes
            .get(coff..coff_end)
            .ok_or(PeError::BadNtOffset { offset: nt })?;

        let machine = PeMachine::from_raw(u16::from_le_bytes([coff_bytes[0], coff_bytes[1]]));
        let section_count = u16::from_le_bytes([coff_bytes[2], coff_bytes[3]]);
        let optional_size = u16::from_le_bytes([coff_bytes[16], coff_bytes[17]]);

        let optional = coff_end;
        let magic = read_u16(bytes, optional).ok_or(PeError::BadNtOffset { offset: nt })?;
        // 0x10b is PE32, 0x20b is PE32+. The data directory sits at a different
        // offset in each, which is the classic place to get this wrong.
        // PE32 stores ImageBase as a u32 at +28; PE32+ as a u64 at +24.
        let (data_directory_offset, image_base) = match magic {
            0x10b => (
                optional + 96,
                u64::from(read_u32(bytes, optional + 28).unwrap_or(0)),
            ),
            0x20b => (optional + 112, read_u64(bytes, optional + 24).unwrap_or(0)),
            other => return Err(PeError::UnknownOptionalMagic { magic: other }),
        };

        // Data directory entry 0 is the export table.
        let export_table = match (
            read_u32(bytes, data_directory_offset),
            read_u32(bytes, data_directory_offset + 4),
        ) {
            (Some(rva), Some(size)) if rva != 0 => Some((rva, size)),
            _ => None,
        };

        let section_table_offset = optional
            .checked_add(optional_size as usize)
            .ok_or(PeError::BadNtOffset { offset: nt })?;

        Ok(Self {
            machine,
            section_count,
            section_table_offset,
            export_table,
            image_base,
        })
    }
}

impl Section {
    fn read(bytes: &[u8], header: &Header, index: u16) -> Result<Self, PeError> {
        const SECTION_ENTRY_SIZE: usize = 40;
        let offset = header.section_table_offset + (index as usize) * SECTION_ENTRY_SIZE;
        let end = offset
            .checked_add(SECTION_ENTRY_SIZE)
            .ok_or(PeError::SectionTableTruncated)?;
        let entry = bytes
            .get(offset..end)
            .ok_or(PeError::SectionTableTruncated)?;

        let raw_name = &entry[0..8];
        let name_end = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
        let name = String::from_utf8_lossy(&raw_name[..name_end]).into_owned();

        // Section header: Name(0..8), VirtualSize(8), VirtualAddress(12),
        // SizeOfRawData(16), PointerToRawData(20).
        Ok(Self {
            name,
            virtual_size: u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]),
            virtual_address: u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]),
            raw_size: u32::from_le_bytes([entry[16], entry[17], entry[18], entry[19]]),
            raw_offset: u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]),
        })
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

/// Errors from parsing a PE image.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PeError {
    #[error("image is too short to contain a PE header ({len} bytes)")]
    TooShort { len: usize },

    #[error("missing MZ signature")]
    MissingDosSignature,

    #[error("missing PE signature")]
    MissingNtSignature,

    #[error("NT header offset {offset} is outside the image")]
    BadNtOffset { offset: usize },

    #[error("unknown optional header magic 0x{magic:04x} (expected PE32 or PE32+)")]
    UnknownOptionalMagic { magic: u16 },

    #[error("section table is truncated")]
    SectionTableTruncated,

    #[error("section {name:?} spans {offset}+{size} which is past the {file_len}-byte image")]
    SectionOutOfRange {
        name: String,
        offset: u32,
        size: u32,
        file_len: usize,
    },

    #[error("export table at RVA {rva} is not mapped by any section")]
    ExportTableNotMapped { rva: u32 },

    #[error("export name at RVA {rva} is not NUL-terminated")]
    UnterminatedExportName { rva: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid PE32+ image.
    ///
    /// Deliberately constructed rather than copied from a real DLL so the
    /// tests can vary one field at a time.
    fn synthetic_pe(machine: u16, with_export: bool) -> Vec<u8> {
        const NT_OFFSET: usize = 0x80;
        const OPTIONAL_SIZE: usize = 0xf0;
        const SECTION_COUNT: usize = 1;
        let section_table = NT_OFFSET + 4 + 20 + OPTIONAL_SIZE;
        let raw_offset = section_table + 40;
        let raw_data = [0u8; 0x40];
        let mut image = vec![0u8; raw_offset + raw_data.len()];

        // DOS header
        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&(NT_OFFSET as u32).to_le_bytes());

        // NT signature + COFF header
        image[NT_OFFSET..NT_OFFSET + 4].copy_from_slice(b"PE\0\0");
        let coff = NT_OFFSET + 4;
        image[coff..coff + 2].copy_from_slice(&machine.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&(SECTION_COUNT as u16).to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&(OPTIONAL_SIZE as u16).to_le_bytes());

        // Optional header (PE32+)
        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        image[optional + 24..optional + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());

        if with_export {
            // Export table: RVA 0x1000, size 0x100
            let data_directory = optional + 112;
            image[data_directory..data_directory + 4].copy_from_slice(&0x1000u32.to_le_bytes());
            image[data_directory + 4..data_directory + 8].copy_from_slice(&0x100u32.to_le_bytes());
        }

        // Section header: `.text`, VA 0x1000, raw at `raw_offset`
        let section = section_table;
        image[section..section + 5].copy_from_slice(b".text");
        image[section + 8..section + 12].copy_from_slice(&0x100u32.to_le_bytes()); // virtual size
        image[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // VA
        image[section + 16..section + 20].copy_from_slice(&(raw_data.len() as u32).to_le_bytes()); // raw size
        image[section + 20..section + 24].copy_from_slice(&(raw_offset as u32).to_le_bytes()); // raw offset

        image
    }

    #[test]
    fn a_valid_pe32_plus_image_parses() {
        let image = PeImage::parse(synthetic_pe(0x8664, false)).expect("parse");
        assert_eq!(image.machine(), PeMachine::Amd64);
        assert_eq!(image.image_base(), 0x1_4000_0000);
        assert_eq!(image.section_names().collect::<Vec<_>>(), vec![".text"]);
    }

    #[test]
    fn the_machine_matches_what_was_written() {
        assert_eq!(
            PeImage::parse(synthetic_pe(0x014c, false))
                .expect("parse")
                .machine(),
            PeMachine::I386
        );
        assert_eq!(
            PeImage::parse(synthetic_pe(0xaa64, false))
                .expect("parse")
                .machine(),
            PeMachine::Arm64
        );
        assert_eq!(
            PeImage::parse(synthetic_pe(0x1234, false))
                .expect("parse")
                .machine(),
            PeMachine::Other(0x1234)
        );
    }

    #[test]
    fn an_rva_resolves_through_the_section_table() {
        let image = PeImage::parse(synthetic_pe(0x8664, false)).expect("parse");
        // Section VA 0x1000 maps to the start of its raw data.
        let offset = image.rva_to_offset(0x1000).expect("mapped");
        assert_eq!(offset, section_raw_offset());
        assert_eq!(
            image.rva_to_offset(0x1010),
            Some(section_raw_offset() + 0x10)
        );
    }

    /// The section table and raw data offsets are a function of the header
    /// sizes, which the test image fixes.
    fn section_raw_offset() -> usize {
        0x80 + 4 + 20 + 0xf0 + 40
    }

    #[test]
    fn an_rva_beyond_every_section_is_not_mapped() {
        let image = PeImage::parse(synthetic_pe(0x8664, false)).expect("parse");
        // The image is only ~0x1d4 bytes; this RVA lands far past it.
        assert!(image.rva_to_offset(0x90_0000).is_none());
    }

    #[test]
    fn a_missing_dos_signature_is_rejected() {
        let mut image = synthetic_pe(0x8664, false);
        image[0] = b'X';
        assert_eq!(
            PeImage::parse(image).expect_err("reject"),
            PeError::MissingDosSignature
        );
    }

    #[test]
    fn a_missing_nt_signature_is_rejected() {
        let mut image = synthetic_pe(0x8664, false);
        image[0x80] = b'X';
        assert_eq!(
            PeImage::parse(image).expect_err("reject"),
            PeError::MissingNtSignature
        );
    }

    #[test]
    fn a_truncated_header_is_rejected() {
        let image = vec![0u8; 8];
        assert!(matches!(
            PeImage::parse(image).expect_err("reject"),
            PeError::TooShort { .. }
        ));
    }

    #[test]
    fn an_nt_offset_past_the_end_is_rejected() {
        let mut image = synthetic_pe(0x8664, false);
        image[0x3c..0x40].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        assert!(matches!(
            PeImage::parse(image).expect_err("reject"),
            PeError::BadNtOffset { .. }
        ));
    }

    #[test]
    fn an_unknown_optional_magic_is_rejected() {
        let mut image = synthetic_pe(0x8664, false);
        let optional = 0x80 + 4 + 20;
        image[optional..optional + 2].copy_from_slice(&0x9999u16.to_le_bytes());
        assert!(matches!(
            PeImage::parse(image).expect_err("reject"),
            PeError::UnknownOptionalMagic { .. }
        ));
    }

    /// The check that makes the loader safe: a section claiming to extend past
    /// the buffer must be refused, not clamped.
    #[test]
    fn a_section_extending_past_the_image_is_rejected() {
        let mut image = synthetic_pe(0x8664, false);
        let section = 0x80 + 4 + 20 + 0xf0;
        // Claim a raw range far larger than the file.
        image[section + 16..section + 20].copy_from_slice(&0xffffu32.to_le_bytes());
        assert!(matches!(
            PeImage::parse(image).expect_err("reject"),
            PeError::SectionOutOfRange { .. }
        ));
    }

    #[test]
    fn an_image_without_an_export_table_exports_nothing() {
        let image = PeImage::parse(synthetic_pe(0x8664, false)).expect("parse");
        assert!(image.exported_names().expect("names").is_empty());
    }

    /// An export directory that points outside every section must be reported,
    /// not silently treated as empty: the two mean different things.
    #[test]
    fn an_unmapped_export_table_is_an_error() {
        let mut image = synthetic_pe(0x8664, true);
        let data_directory = 0x80 + 4 + 20 + 112;
        image[data_directory..data_directory + 4].copy_from_slice(&0x90_0000u32.to_le_bytes());
        let parsed = PeImage::parse(image).expect("parse");
        assert!(matches!(
            parsed.exported_names().expect_err("reject"),
            PeError::ExportTableNotMapped { .. }
        ));
    }

    #[test]
    fn machine_display_is_human_readable() {
        assert_eq!(PeMachine::Amd64.to_string(), "x86-64");
        assert_eq!(PeMachine::Other(0x1234).to_string(), "machine 0x1234");
    }

    #[test]
    fn the_current_process_machine_matches_itself() {
        assert!(PeMachine::current_process().matches_current_process());
    }
}
