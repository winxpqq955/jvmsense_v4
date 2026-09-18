//! Serving exports for modules the OS loader has never seen.
//!
//! A memory-loaded library is handed to the JVM as a synthetic `HMODULE`
//! (see [`crate::native::win32_library`]). The JVM then resolves its symbols
//! with `GetProcAddress` on that handle, which the OS cannot satisfy — it has
//! no record of the module. This module answers those lookups from the mapped
//! image's own export table.
//!
//! Mapping handle index to module lives here rather than in the hook module so
//! that the hooks stay free of any knowledge about how a module is represented.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;

/// The modules handed out as synthetic handles, in handle-index order.
static MODULES: OnceLock<parking_lot::Mutex<HashMap<usize, Box<dyn ModuleExports>>>> =
    OnceLock::new();

/// What a mapped module must be able to answer.
pub trait ModuleExports: Send + Sync {
    /// The address of an exported symbol, if the module has one.
    fn proc_address(&self, symbol: &str) -> Option<*mut c_void>;

    /// The module's path, for diagnostics.
    fn path(&self) -> &str;
}

fn modules() -> &'static parking_lot::Mutex<HashMap<usize, Box<dyn ModuleExports>>> {
    MODULES.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Register a module's export table under a handle index.
///
/// The simpler form of [`register`], for callers that have already collected
/// the exports into a name-to-address map — which is what a freshly mapped PE
/// image produces.
///
/// # Errors
///
/// Returns `Err` if the index is already taken.
pub fn register(index: usize, path: String, exports: HashMap<String, usize>) -> Result<(), String> {
    let mut modules = modules().lock();
    if modules.contains_key(&index) {
        return Err(format!("handle index {index} is already registered"));
    }
    modules.insert(index, Box::new(FixedExports { path, exports }));
    Ok(())
}

/// A module whose exports were collected up front.
struct FixedExports {
    path: String,
    exports: HashMap<String, usize>,
}

impl ModuleExports for FixedExports {
    fn proc_address(&self, symbol: &str) -> Option<*mut c_void> {
        self.exports.get(symbol).map(|value| *value as *mut c_void)
    }

    fn path(&self) -> &str {
        &self.path
    }
}

/// Register a module under a handle index, returning that index.
///
/// # Errors
///
/// Returns `Err` if the index is already taken, which would mean the caller
/// reused one and a lookup could resolve against the wrong module.
///
/// Resolve a symbol for a synthetic handle.
///
/// Tries the requested spelling first, then the platform's undecorated form,
/// because a JNI entry point is frequently asked for as `_JNI_OnLoad@8` while
/// the export table carries `JNI_OnLoad`.
#[must_use]
pub fn resolve_synthetic_export(index: usize, symbol: &str) -> Option<*mut c_void> {
    let modules = modules().lock();
    let module = modules.get(&index)?;

    if let Some(address) = module.proc_address(symbol) {
        return Some(address);
    }
    let undecorated = crate::native::lib::memload::strip_call_decoration(symbol);
    if undecorated != symbol {
        return module.proc_address(undecorated);
    }
    None
}

/// The path of a registered module, for error messages.
#[must_use]
pub fn synthetic_module_path(index: usize) -> Option<String> {
    modules().lock().get(&index).map(|m| m.path().to_string())
}

/// How many modules are registered.
#[must_use]
pub fn registered_count() -> usize {
    modules().lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an export map the way a mapped image does.
    fn exports(symbols: &[(&str, usize)]) -> HashMap<String, usize> {
        symbols
            .iter()
            .map(|(name, value)| ((*name).to_string(), *value))
            .collect()
    }

    #[test]
    fn a_registered_export_is_resolved() {
        let index = 9001;
        register(index, "test.dll".into(), exports(&[("my_symbol", 0x1234)])).expect("register");

        assert_eq!(
            resolve_synthetic_export(index, "my_symbol"),
            Some(0x1234 as *mut c_void)
        );
    }

    #[test]
    fn a_decorated_name_resolves_to_its_undecorated_export() {
        let index = 9002;
        register(index, "test.dll".into(), exports(&[("JNI_OnLoad", 0x5678)])).expect("register");

        assert_eq!(
            resolve_synthetic_export(index, "_JNI_OnLoad@8"),
            Some(0x5678 as *mut c_void),
            "a JNI entry point is commonly requested with stdcall decoration"
        );
    }

    #[test]
    fn an_unknown_symbol_resolves_to_nothing() {
        let index = 9003;
        register(index, "test.dll".into(), exports(&[("present", 1)])).expect("register");

        assert_eq!(resolve_synthetic_export(index, "absent"), None);
    }

    #[test]
    fn an_unregistered_index_resolves_to_nothing() {
        assert_eq!(resolve_synthetic_export(999_999, "anything"), None);
    }

    #[test]
    fn a_duplicate_index_is_refused() {
        let index = 9004;
        register(index, "a.dll".into(), exports(&[])).expect("first");
        assert!(
            register(index, "b.dll".into(), exports(&[])).is_err(),
            "reusing an index would let a lookup resolve against the wrong module"
        );
    }

    #[test]
    fn a_module_path_is_reported() {
        let index = 9005;
        register(index, "reported.dll".into(), exports(&[])).expect("register");
        assert_eq!(
            synthetic_module_path(index).as_deref(),
            Some("reported.dll")
        );
    }
}
