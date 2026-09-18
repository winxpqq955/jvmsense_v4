//! Errors produced while sizing a method body.

use thiserror::Error;

/// Failure modes of [`crate::bytecode::frame_size`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BytecodeError {
    /// The instruction stream popped an operand that was never pushed.
    ///
    /// This means either the stream is malformed or this computer has a bug.
    /// Either way the computed depth would be meaningless, so we refuse to
    /// return a number rather than emit a class that fails verification.
    #[error("operand stack underflow: {opcode} needs {needed} slot(s) but depth is {depth}")]
    DepthUnderflow {
        opcode: String,
        needed: u16,
        depth: u16,
    },

    /// The operand stack grew past `u16::MAX`, which the class file format
    /// cannot encode.
    #[error("operand stack depth {depth} exceeds the {limit} the class file format allows")]
    StackOverflow { depth: u32, limit: u16 },

    /// A descriptor was not a valid JVM field or method descriptor.
    #[error("malformed descriptor: {descriptor:?}")]
    MalformedDescriptor { descriptor: String },
}
