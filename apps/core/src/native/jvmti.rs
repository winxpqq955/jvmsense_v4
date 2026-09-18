//! Minimal JVMTI access for the post-launch fallback.
//!
//! The JNI crate exposes the JNI invocation interface but not JVMTI. The two
//! operations needed to redefine an already-loaded target are stable ABI entries:
//! `AddCapabilities(can_redefine_classes)` (slot 141) and
//! `RedefineClasses` (slot 86). This module reads those slots from the
//! `jvmtiEnv` function table and keeps the raw environment private.

use std::ffi::c_void;

use jni::objects::JClass;
use jni::sys::{jint, jobject};
use jni::JavaVM;

/// JVMTI 1.2, as defined in `jvmti.h`.
const JVMTI_VERSION_1_2: jint = 0x3001_0200;
/// `JVMTI_ERROR_NONE`.
const JVMTI_ERROR_NONE: jint = 0;
/// The `can_redefine_classes` bit in `jvmtiCapabilities` (bit 9).
const CAN_REDEFINE_CLASSES_BIT: usize = 9;

/// Function-table slot for `RedefineClasses`.
const SLOT_REDEFINE_CLASSES: usize = 86;
/// Function-table slot for `AddCapabilities`.
const SLOT_ADD_CAPABILITIES: usize = 141;

type AddCapabilitiesFn = unsafe extern "system" fn(*mut c_void, *const JvmtiCapabilities) -> jint;
type RedefineClassesFn =
    unsafe extern "system" fn(*mut c_void, jint, *const JvmtiClassDefinition) -> jint;

#[repr(C)]
struct JvmtiCapabilities {
    words: [u32; 5],
}

#[repr(C)]
struct JvmtiClassDefinition {
    klass: jobject,
    class_byte_count: jint,
    class_bytes: *const u8,
}

/// One class redefinition request.
pub struct ClassRedefinition<'a> {
    /// The class object currently loaded by the JVM.
    pub class: &'a JClass<'a>,
    /// Replacement bytecode, with the same class name and schema.
    pub bytes: &'a [u8],
}

/// A live JVMTI environment.
pub struct Jvmti {
    env: *mut c_void,
    functions: *const usize,
}

impl std::fmt::Debug for Jvmti {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jvmti").finish_non_exhaustive()
    }
}

impl Jvmti {
    /// Attach to the current JVM through its invocation interface.
    ///
    /// # Errors
    ///
    /// Returns [`JvmtiError::GetEnv`] if the JVM does not expose a JVMTI 1.2
    /// environment, or [`JvmtiError::MissingSlot`] if the function table is
    /// truncated.
    ///
    /// # Safety
    ///
    /// The `JavaVM` must remain alive for the lifetime of the returned value.
    pub unsafe fn from_vm(vm: &JavaVM) -> Result<Self, JvmtiError> {
        let vm_ptr = vm.get_java_vm_pointer();
        let invoke = *vm_ptr;
        let get_env = (*invoke).GetEnv.ok_or(JvmtiError::MissingSlot("GetEnv"))?;
        let mut env: *mut c_void = std::ptr::null_mut();
        let rc = get_env(vm_ptr, &mut env, JVMTI_VERSION_1_2);
        if rc != JNI_OK {
            return Err(JvmtiError::GetEnv { code: rc });
        }
        if env.is_null() {
            return Err(JvmtiError::GetEnv { code: rc });
        }

        // jvmtiEnv's first field is its function table.
        let functions = *env.cast::<*const usize>();
        if functions.is_null() {
            return Err(JvmtiError::MissingSlot("jvmtiInterface_1_"));
        }

        Ok(Self { env, functions })
    }

    /// Add the capability required by [`Self::redefine_classes`].
    ///
    /// # Errors
    ///
    /// Returns [`JvmtiError::Jvmti`] with the JVMTI error code when the VM
    /// refuses the capability.
    pub fn enable_class_redefinition(&self) -> Result<(), JvmtiError> {
        let add = self.function::<AddCapabilitiesFn>(SLOT_ADD_CAPABILITIES)?;
        let mut capabilities = JvmtiCapabilities { words: [0; 5] };
        capabilities.words[CAN_REDEFINE_CLASSES_BIT / 32] |= 1 << (CAN_REDEFINE_CLASSES_BIT % 32);
        let rc = unsafe { add(self.env, &capabilities) };
        check("AddCapabilities", rc)
    }

    /// Redefine already-loaded classes with replacement bytecode.
    ///
    /// # Errors
    ///
    /// Returns [`JvmtiError::Jvmti`] with the JVMTI error code. Common codes
    /// include `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_SCHEMA_CHANGED` (62) and
    /// `JVMTI_ERROR_UNMODIFIABLE_CLASS` (79); callers should include the class
    /// name in their own error context.
    pub fn redefine_classes(
        &self,
        definitions: &[ClassRedefinition<'_>],
    ) -> Result<(), JvmtiError> {
        if definitions.is_empty() {
            return Ok(());
        }

        let redefine = self.function::<RedefineClassesFn>(SLOT_REDEFINE_CLASSES)?;
        let raw: Vec<JvmtiClassDefinition> = definitions
            .iter()
            .map(|definition| JvmtiClassDefinition {
                klass: definition.class.as_raw(),
                class_byte_count: i32::try_from(definition.bytes.len()).unwrap_or(i32::MAX),
                class_bytes: definition.bytes.as_ptr(),
            })
            .collect();
        let count = i32::try_from(raw.len()).map_err(|_| JvmtiError::TooManyClasses)?;
        let rc = unsafe { redefine(self.env, count, raw.as_ptr()) };
        check("RedefineClasses", rc)
    }

    fn function<T: Copy>(&self, slot: usize) -> Result<T, JvmtiError> {
        let address = unsafe { *self.functions.add(slot) };
        if address == 0 {
            return Err(JvmtiError::MissingSlot("jvmti function"));
        }
        Ok(unsafe { std::mem::transmute_copy::<usize, T>(&address) })
    }
}

const JNI_OK: jint = 0;

fn check(operation: &'static str, code: jint) -> Result<(), JvmtiError> {
    if code == JVMTI_ERROR_NONE {
        Ok(())
    } else {
        Err(JvmtiError::Jvmti { operation, code })
    }
}

/// Errors from the JVMTI bridge.
#[derive(Debug, thiserror::Error)]
pub enum JvmtiError {
    /// The invocation interface could not return a JVMTI environment.
    #[error("JVM GetEnv(JVMTI 1.2) failed with JNI code {code}")]
    GetEnv {
        /// The JNI error code.
        code: jint,
    },

    /// A JVMTI table slot was absent.
    #[error("JVMTI function table is missing {0}")]
    MissingSlot(&'static str),

    /// JVMTI returned a non-zero error code.
    #[error("JVMTI {operation} failed with error {code}")]
    Jvmti {
        /// The JVMTI operation.
        operation: &'static str,
        /// The JVMTI error code.
        code: jint,
    },

    /// More definitions than the JVMTI API can encode in one call.
    #[error("too many classes in one redefinition batch")]
    TooManyClasses,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_word_has_the_expected_shape() {
        assert_eq!(std::mem::size_of::<JvmtiCapabilities>(), 20);
        assert_eq!(SLOT_REDEFINE_CLASSES, 86);
        assert_eq!(SLOT_ADD_CAPABILITIES, 141);
    }
}
