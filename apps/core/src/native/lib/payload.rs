//! Decoding a native library payload into a verified PE image.
//!
//! A manifest entry describes a native library the same way it describes a
//! jar: a payload file, a storage encoding, a SHA-256 and a decoded size. The
//! difference is what happens after verification — a jar is indexed, a DLL is
//! parsed as a PE image and then mapped into the process.
//!
//! Verification is not optional here. A memory-loaded DLL is executed with the
//! full privileges of the launcher, and it never passes through the OS loader
//! or any code-signing check, so the digest is the only integrity gate there
//! is.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::artifact::StorageEncoding;
use crate::native::lib::pe::{PeError, PeImage};
use crate::native::lib::ManagedLibrary;

/// One native library as the manifest declares it.
#[derive(Debug, Clone)]
pub struct NativePayload {
    /// The file name the JVM will ask for, e.g. `lwjgl_opengl.dll`.
    pub file_name: String,
    /// Where the encoded payload lives.
    pub payload_path: PathBuf,
    /// How the payload is encoded.
    pub encoding: StorageEncoding,
    /// Expected SHA-256 of the *decoded* image, lowercase hex.
    pub sha256_hex: String,
    /// Expected decoded length.
    pub size_bytes: u64,
}

/// Decode, verify and parse a payload into a library ready to map.
///
/// # Errors
///
/// Returns [`PayloadError`] for any failure. The order of the checks is
/// deliberate: size, then digest, then PE structure. Each is cheaper than the
/// next, and reporting the earliest failure gives the most specific message —
/// a size mismatch means the manifest and the payload are from different
/// builds, which is a different problem from a corrupt DLL.
pub fn decode_payload(payload: &NativePayload) -> Result<ManagedLibrary, PayloadError> {
    let encoded = std::fs::read(&payload.payload_path).map_err(|source| PayloadError::Io {
        path: payload.payload_path.clone(),
        source,
    })?;

    let decoded = match payload.encoding {
        StorageEncoding::Identity => encoded,
        StorageEncoding::Zstd => {
            zstd::stream::decode_all(encoded.as_slice()).map_err(|source| PayloadError::Decode {
                name: payload.file_name.clone(),
                path: payload.payload_path.clone(),
                source,
            })?
        }
    };

    if decoded.len() as u64 != payload.size_bytes {
        return Err(PayloadError::SizeMismatch {
            name: payload.file_name.clone(),
            declared: payload.size_bytes,
            actual: decoded.len(),
        });
    }

    let digest = hex_lower(&Sha256::digest(&decoded));
    let expected = payload.sha256_hex.to_ascii_lowercase();
    if digest != expected {
        return Err(PayloadError::HashMismatch {
            name: payload.file_name.clone(),
            declared: expected,
            actual: digest,
        });
    }

    ManagedLibrary::new(payload.file_name.clone(), decoded).map_err(|source| {
        PayloadError::NotPortableExecutable {
            name: payload.file_name.clone(),
            source,
        }
    })
}

/// Decode and verify a payload without parsing it as a PE image.
///
/// Used for the non-`.dll` entries a manifest may carry, where the bytes are
/// served rather than mapped.
///
/// # Errors
///
/// See [`PayloadError`].
pub fn decode_bytes(payload: &NativePayload) -> Result<Vec<u8>, PayloadError> {
    let encoded = std::fs::read(&payload.payload_path).map_err(|source| PayloadError::Io {
        path: payload.payload_path.clone(),
        source,
    })?;
    let decoded = match payload.encoding {
        StorageEncoding::Identity => encoded,
        StorageEncoding::Zstd => {
            zstd::stream::decode_all(encoded.as_slice()).map_err(|source| PayloadError::Decode {
                name: payload.file_name.clone(),
                path: payload.payload_path.clone(),
                source,
            })?
        }
    };
    if decoded.len() as u64 != payload.size_bytes {
        return Err(PayloadError::SizeMismatch {
            name: payload.file_name.clone(),
            declared: payload.size_bytes,
            actual: decoded.len(),
        });
    }
    let digest = hex_lower(&Sha256::digest(&decoded));
    let expected = payload.sha256_hex.to_ascii_lowercase();
    if digest != expected {
        return Err(PayloadError::HashMismatch {
            name: payload.file_name.clone(),
            declared: expected,
            actual: digest,
        });
    }
    Ok(decoded)
}

/// Lowercase hex encoding of a digest.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Errors from decoding a native payload.
#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    #[error("cannot read payload for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot decompress zstd payload for {name} ({path}): {source}")]
    Decode {
        name: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{name}: declared {declared} bytes but decoded to {actual}")]
    SizeMismatch {
        name: String,
        declared: u64,
        actual: usize,
    },

    #[error("{name}: declared sha256 {declared} but computed {actual}")]
    HashMismatch {
        name: String,
        declared: String,
        actual: String,
    },

    #[error("{name} is not a usable PE image: {source}")]
    NotPortableExecutable {
        name: String,
        #[source]
        source: PeError,
    },
}

/// The arch a payload targets, without decoding it.
///
/// Cheap enough to run on every entry at startup so a mixed-architecture
/// manifest can be reported up front rather than failing when the JVM first
/// asks for the mismatched library.
///
/// # Errors
///
/// See [`PayloadError`].
pub fn peek_machine(
    payload: &NativePayload,
) -> Result<crate::native::lib::pe::PeMachine, PayloadError> {
    let encoded = std::fs::read(&payload.payload_path).map_err(|source| PayloadError::Io {
        path: payload.payload_path.clone(),
        source,
    })?;
    let decoded = match payload.encoding {
        StorageEncoding::Identity => encoded,
        StorageEncoding::Zstd => {
            zstd::stream::decode_all(encoded.as_slice()).map_err(|source| PayloadError::Decode {
                name: payload.file_name.clone(),
                path: payload.payload_path.clone(),
                source,
            })?
        }
    };
    PeImage::parse(decoded)
        .map(|image| image.machine())
        .map_err(|source| PayloadError::NotPortableExecutable {
            name: payload.file_name.clone(),
            source,
        })
}

/// Convenience: an identity-encoded payload spec for a file, hashing it now.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the file cannot be read.
pub fn identity_payload(
    file_name: impl Into<String>,
    path: &Path,
) -> Result<NativePayload, std::io::Error> {
    let bytes = std::fs::read(path)?;
    Ok(NativePayload {
        file_name: file_name.into(),
        payload_path: path.to_path_buf(),
        encoding: StorageEncoding::Identity,
        sha256_hex: hex_lower(&Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PE32+ image, enough to pass the parser.
    fn synthetic_dll(machine: u16) -> Vec<u8> {
        const NT_OFFSET: usize = 0x80;
        const OPTIONAL_SIZE: usize = 0xf0;
        let section_table = NT_OFFSET + 4 + 20 + OPTIONAL_SIZE;
        let raw_offset = section_table + 40;
        let mut image = vec![0u8; raw_offset + 0x40];

        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&(NT_OFFSET as u32).to_le_bytes());
        image[NT_OFFSET..NT_OFFSET + 4].copy_from_slice(b"PE\0\0");

        let coff = NT_OFFSET + 4;
        image[coff..coff + 2].copy_from_slice(&machine.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&(OPTIONAL_SIZE as u16).to_le_bytes());

        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        image[optional + 24..optional + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());

        let section = section_table;
        image[section..section + 5].copy_from_slice(b".text");
        image[section + 8..section + 12].copy_from_slice(&0x100u32.to_le_bytes());
        image[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        image[section + 16..section + 20].copy_from_slice(&0x40u32.to_le_bytes());
        image[section + 20..section + 24].copy_from_slice(&(raw_offset as u32).to_le_bytes());

        image
    }

    /// Write `bytes` as a payload and return a spec that declares them.
    fn payload_for(
        dir: &Path,
        name: &str,
        bytes: &[u8],
        encoding: StorageEncoding,
    ) -> NativePayload {
        let path = dir.join(name);
        let stored = match encoding {
            StorageEncoding::Identity => bytes.to_vec(),
            StorageEncoding::Zstd => zstd::stream::encode_all(bytes, 3).expect("compress"),
        };
        std::fs::write(&path, &stored).expect("write payload");
        NativePayload {
            file_name: name.to_string(),
            payload_path: path,
            encoding,
            sha256_hex: hex_lower(&Sha256::digest(bytes)),
            size_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn an_identity_payload_decodes_to_a_managed_library() {
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = payload_for(
            dir.path(),
            "lwjgl_opengl.dll",
            &synthetic_dll(0x8664),
            StorageEncoding::Identity,
        );

        let library = decode_payload(&payload).expect("decode");

        assert_eq!(library.file_name(), "lwjgl_opengl.dll");
        assert_eq!(library.stem(), "lwjgl_opengl");
        assert!(library.matches_request("lwjgl_opengl"));
    }

    #[test]
    fn a_zstd_payload_decodes_to_the_same_image() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image = synthetic_dll(0x8664);
        let payload = payload_for(dir.path(), "a.dll", &image, StorageEncoding::Zstd);

        let library = decode_payload(&payload).expect("decode");

        assert_eq!(library.image().len(), image.len());
    }

    #[test]
    fn a_corrupted_payload_is_rejected_on_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image = synthetic_dll(0x8664);
        let mut payload = payload_for(dir.path(), "a.dll", &image, StorageEncoding::Identity);
        // Corrupt a byte well inside the image, keeping the length the same so
        // the size check passes and the digest check is what rejects it.
        let mut stored = image.clone();
        stored[0x100] ^= 0xff;
        std::fs::write(&payload.payload_path, &stored).expect("rewrite");
        payload.size_bytes = stored.len() as u64;

        let error = decode_payload(&payload).expect_err("reject");

        assert!(matches!(error, PayloadError::HashMismatch { .. }));
    }

    #[test]
    fn a_wrong_declared_size_is_rejected_before_hashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut payload = payload_for(
            dir.path(),
            "a.dll",
            &synthetic_dll(0x8664),
            StorageEncoding::Identity,
        );
        payload.size_bytes = 1;

        let error = decode_payload(&payload).expect_err("reject");

        assert!(matches!(error, PayloadError::SizeMismatch { .. }));
    }

    #[test]
    fn bytes_that_are_not_a_pe_image_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = payload_for(
            dir.path(),
            "not-a-dll.dll",
            b"definitely not a PE image",
            StorageEncoding::Identity,
        );

        let error = decode_payload(&payload).expect_err("reject");

        assert!(matches!(error, PayloadError::NotPortableExecutable { .. }));
    }

    #[test]
    fn a_missing_payload_file_reports_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = NativePayload {
            file_name: "gone.dll".into(),
            payload_path: dir.path().join("missing.dll"),
            encoding: StorageEncoding::Identity,
            sha256_hex: "00".repeat(32),
            size_bytes: 0,
        };

        assert!(matches!(
            decode_payload(&payload).expect_err("reject"),
            PayloadError::Io { .. }
        ));
    }

    #[test]
    fn machine_can_be_peeked_without_full_verification() {
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = payload_for(
            dir.path(),
            "a.dll",
            &synthetic_dll(0x014c),
            StorageEncoding::Identity,
        );

        assert_eq!(
            peek_machine(&payload).expect("peek"),
            crate::native::lib::pe::PeMachine::I386
        );
    }

    #[test]
    fn decode_bytes_verifies_without_requiring_a_pe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = payload_for(
            dir.path(),
            "data.bin",
            b"arbitrary bytes",
            StorageEncoding::Zstd,
        );

        assert_eq!(decode_bytes(&payload).expect("decode"), b"arbitrary bytes");
    }

    #[test]
    fn decode_bytes_still_checks_the_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut payload = payload_for(
            dir.path(),
            "data.bin",
            b"arbitrary bytes",
            StorageEncoding::Identity,
        );
        payload.sha256_hex = "00".repeat(32);

        assert!(matches!(
            decode_bytes(&payload).expect_err("reject"),
            PayloadError::HashMismatch { .. }
        ));
    }

    #[test]
    fn identity_payload_helper_hashes_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.dll");
        std::fs::write(&path, b"contents").expect("write");

        let payload = identity_payload("x.dll", &path).expect("payload");

        assert_eq!(payload.size_bytes, 8);
        assert_eq!(payload.sha256_hex, hex_lower(&Sha256::digest(b"contents")));
    }
}
