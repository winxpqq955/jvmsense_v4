//! jvmsense-core — in-memory Java application launcher and protection core.
//!
//! The design goal is that no Java bytecode an application needs ever reaches
//! disk in analysable form. The mechanism is a *hollow-path* virtual file
//! system: every artifact exists on disk as a zero-length placeholder, the
//! real bytes live only in this process's memory, and native hooks serve every
//! read of a placeholder from those bytes. See `spikes/FINDINGS.md` for the
//! probes that established this is viable, and why the alternatives are not.
//!
//! The primary correctness path is launch-time Fabric preparation in
//! [`launch`]: mods are parsed, flattened/remapped, mounted as hollow jars, and
//! exposed through Fabric's explicit mod list before the JVM exists. Mixin then
//! transforms targets during their first class load. [`runtime`] and its JVMTI
//! bridge are the explicit fallback for targets that were already loaded before
//! jvmsense could intervene; [`bytecode`] supports safe fallback transformations.

#![deny(unused_crate_dependencies)]

// Dependencies land here as their modules are written. Declaring them up front
// keeps Cargo.toml stable while the crate is built out module by module; the
// `unused_crate_dependencies` deny above keeps this list honest.
use anyhow as _;
use dashmap as _;
use parking_lot as _;
use rand as _;
use ristretto_classfile as _;
use rustls as _;
use rustls_native_certs as _;
use serde as _;
use serde_json as _;
use zip as _;

pub mod artifact;
pub mod bytecode;
pub mod jdk;
pub mod launch;
pub mod native;
pub mod remap;
pub mod runtime;
pub mod vfs;
