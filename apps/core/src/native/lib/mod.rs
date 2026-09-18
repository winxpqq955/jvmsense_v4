//! Loading native libraries from memory.
//!
//! Minecraft and Fabric both depend on native code: LWJGL's OpenGL, GLFW and
//! OpenAL bindings, plus whatever a mod ships. Those normally arrive as `.dll`
//! files that the OS loader maps from disk, which would put analysable binaries
//! on the filesystem and defeat the point of the rest of this crate.
//!
//! So the DLLs are decoded from the manifest's payloads, verified, and mapped
//! into the process directly — no file is written, and the OS loader is
//! bypassed entirely. The interception points are the JVM's own library-loading
//! natives, which is why `symbols::NATIVE_LOAD_SYMBOLS` lists both the
//! `NativeLibraries` and `RawNativeLibraries` families: JDK 21 added the second
//! as a path behind `System.load`, and hooking only the first would leave a
//! bypass through which a library loaded normally.
//!
//! This module is split so that the parts worth testing are testable without a
//! JVM: [`pe`] parses and validates the image, [`payload`] decodes it, and only
//! [`memload`] needs Windows.

pub mod payload;
pub mod pe;

#[cfg(windows)]
pub mod memload;

#[cfg(windows)]
pub mod synthetic;

#[cfg(windows)]
pub use synthetic::{register as register_synthetic_module, resolve_synthetic_export};

#[cfg(windows)]
pub use memload::{map_image, LoadError, LoadedModule};

pub use payload::{decode_payload, NativePayload, PayloadError};
pub use pe::{PeError, PeImage, PeMachine};

/// A native library the launcher knows how to serve.
///
/// This is the resolved form of a manifest's `native_libraries` entry: the
/// decoded bytes plus the names the JVM might ask for it by.
#[derive(Debug, Clone)]
pub struct ManagedLibrary {
    /// The file name as the manifest gave it, e.g. `lwjgl_opengl.dll`.
    file_name: String,
    /// The name without its extension, which is how `System.loadLibrary` refers
    /// to it.
    stem: String,
    /// The verified decoded image.
    image: PeImage,
}

impl ManagedLibrary {
    /// Build from a decoded payload.
    ///
    /// # Errors
    ///
    /// Returns [`PeError`] if the bytes are not a usable PE image. Refusing
    /// here is deliberate: a library that cannot be parsed would otherwise fail
    /// much later, inside `JNI_OnLoad`, with nothing useful in the message.
    pub fn new(file_name: impl Into<String>, bytes: Vec<u8>) -> Result<Self, PeError> {
        let file_name = file_name.into();
        let stem = file_name
            .rsplit_once('.')
            .map_or(file_name.clone(), |(stem, _)| stem.to_string());
        let image = PeImage::parse(bytes)?;
        Ok(Self {
            file_name,
            stem,
            image,
        })
    }

    /// The file name as given.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The name without its extension.
    #[must_use]
    pub fn stem(&self) -> &str {
        &self.stem
    }

    /// The parsed image.
    #[must_use]
    pub fn image(&self) -> &PeImage {
        &self.image
    }

    /// Whether this library is a candidate for a name the JVM asked for.
    ///
    /// Windows resolves library names case-insensitively and tolerates a
    /// missing `.dll`, so the comparison is normalized on both sides.
    #[must_use]
    pub fn matches_request(&self, request: &str) -> bool {
        let request = normalize_library_request(request);
        let stem = normalize_library_request(&self.stem);
        let file = normalize_library_request(&self.file_name);
        request == stem || request == file
    }
}

/// Normalize a library name for comparison: path to its last component,
/// lowercase, `.dll` stripped.
#[must_use]
pub fn normalize_library_request(request: &str) -> String {
    let last = request
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(request)
        .to_ascii_lowercase();
    last.strip_suffix(".dll")
        .map_or(last.clone(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_matches_by_stem_or_full_name() {
        // Built through the parser only in the integration test; here the
        // matching rule is exercised directly.
        assert_eq!(normalize_library_request("lwjgl_opengl"), "lwjgl_opengl");
        assert_eq!(
            normalize_library_request("lwjgl_opengl.dll"),
            "lwjgl_opengl"
        );
        assert_eq!(
            normalize_library_request(r"C:\Windows\System32\LWJGL_OpenGL.DLL"),
            "lwjgl_opengl"
        );
        assert_eq!(normalize_library_request("lib/lwjgl.dll"), "lwjgl");
    }

    #[test]
    fn an_unrelated_name_does_not_normalize_to_a_known_one() {
        assert_ne!(
            normalize_library_request("lwjgl_glfw"),
            normalize_library_request("lwjgl_opengl")
        );
    }
}
