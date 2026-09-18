//! Starting a Java application, with its bytes served from memory.
//!
//! [`fabric`] covers the Fabric path: preparing a layout, choosing the system
//! properties, and mounting the game jar and mods at virtual paths. A
//! plain (non-Fabric) application needs no such preparation — it is just a
//! classpath of virtual paths and a main class — so it has no module of its own.

pub mod fabric;
pub mod session;

pub use fabric::{
    prepare_fabric_mod, ClasspathEntry, FabricApplication, FabricLaunchImages, FabricLayout,
    FabricModImage, FabricMountError, PreparedFabricMods,
};
pub use session::{
    create_session_directory, request_from_fabric, run, LaunchError, LaunchOutcome, LaunchRequest,
};
