//! Stack-depth and local-slot computation for generated and rewritten classes.
//!
//! This module exists because `ristretto_classfile` 0.29's own computation is
//! wrong: `Instruction::stack_delta` discards the parsed return type and
//! computes `-parameters.len()` for every invoke form, so any invoke that
//! returns a non-void value is off by one (or two for `long`/`double`).
//! Worse, the crate's own `ClassFile::verify()` accepts the wrong values, so a
//! "generate then verify" pipeline cannot catch it. See `spikes/FINDINGS.md`
//! (V7) for the measured blast radius.
//!
//! We therefore compute both values ourselves, from the instruction stream,
//! and use these numbers in place of ristretto's. The rules implemented here
//! are the JVM specification's operand-stack effects:
//!
//! - `<JVMS §2.11.1>`: `long` and `double` occupy two slots, in locals and on
//!   the stack, even though the instruction operands are single indices.
//! - `<JVMS §6.5>`: each instruction's stack effect, parameterized by the
//!   constant-pool descriptors for field and method references.
//!
//! The computer is deliberately independent of `ristretto_classfile`'s data
//! structures where practical, so that a crate upgrade cannot silently change
//! our answers. It takes a small [`InstructionView`] abstraction that the
//! caller adapts from whatever instruction type it holds.

use crate::bytecode::error::BytecodeError;

pub mod error;
mod locals;
mod stack;
pub mod view;

pub use locals::max_locals;
pub use stack::max_stack;
pub use view::{Descriptor, InstructionView, InvokeKind};

/// Both sizing values for one method body, computed together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSize {
    pub max_stack: u16,
    pub max_locals: u16,
}

/// Compute `max_stack` and `max_locals` for a method body.
///
/// `is_static` matters because a non-static method's local slot 0 holds `this`.
/// `descriptor` is the method's own descriptor, used to size the incoming
/// argument slots.
///
/// # Errors
///
/// Returns [`BytecodeError::DepthUnderflow`] if the instruction stream pops
/// more than it has pushed — which means either the stream is malformed or
/// this computer has a bug; either way the answer would be garbage, so we
/// refuse to guess.
///
/// [`BytecodeError::DepthUnderflow`]: error::BytecodeError::DepthUnderflow
pub fn frame_size(
    instructions: &[InstructionView],
    descriptor: &Descriptor,
    is_static: bool,
) -> Result<FrameSize, BytecodeError> {
    let max_stack = stack::max_stack(instructions)?;
    let max_locals = locals::max_locals(instructions, descriptor, is_static)?;
    Ok(FrameSize {
        max_stack,
        max_locals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use view::{InvokeKind, Opcode};

    /// The exact sequence from the predecessor's generated class loader, which
    /// it had to hand-patch to `max_stack = 2`:
    ///
    /// ```text
    ///   aload_0
    ///   invokestatic  java/lang/ClassLoader.getSystemClassLoader()Ljava/lang/ClassLoader;
    ///   invokespecial java/lang/ClassLoader.<init>(Ljava/lang/ClassLoader;)V
    ///   return
    /// ```
    ///
    /// ristretto computes 1 here; the truth is 2. This test is the regression
    /// guard for that discrepancy.
    #[test]
    fn ctor_with_get_system_class_loader_needs_depth_two() {
        let instructions = vec![
            InstructionView::simple(Opcode::Aload(0)),
            InstructionView::invoke(
                InvokeKind::Static,
                Descriptor::parse("()Ljava/lang/ClassLoader;").expect("descriptor"),
            ),
            InstructionView::invoke(
                InvokeKind::Special,
                Descriptor::parse("(Ljava/lang/ClassLoader;)V").expect("descriptor"),
            ),
            InstructionView::simple(Opcode::Return),
        ];

        let size = frame_size(
            &instructions,
            &Descriptor::parse("()V").expect("descriptor"),
            false,
        )
        .expect("compute");

        assert_eq!(size.max_stack, 2, "ristretto would say 1 here");
        assert_eq!(size.max_locals, 1, "just `this`");
    }

    /// A method whose body is empty still reserves slots for its parameters.
    #[test]
    fn empty_static_method_reserves_parameter_slots() {
        let instructions = vec![InstructionView::simple(Opcode::Return)];
        let size = frame_size(
            &instructions,
            &Descriptor::parse("(IJ)V").expect("descriptor"),
            true,
        )
        .expect("compute");

        assert_eq!(size.max_stack, 0);
        // int = 1 slot, long = 2 slots
        assert_eq!(size.max_locals, 3);
    }

    #[test]
    fn instance_method_reserves_a_slot_for_this() {
        let instructions = vec![InstructionView::simple(Opcode::Return)];
        let size = frame_size(
            &instructions,
            &Descriptor::parse("()V").expect("descriptor"),
            false,
        )
        .expect("compute");

        assert_eq!(size.max_locals, 1);
    }

    #[test]
    fn underflow_is_rejected_rather_than_guessed() {
        // `ireturn` with nothing on the stack.
        let instructions = vec![InstructionView::simple(Opcode::Ireturn)];
        let err = frame_size(
            &instructions,
            &Descriptor::parse("()I").expect("descriptor"),
            true,
        )
        .expect_err("must reject underflow");

        assert!(matches!(err, BytecodeError::DepthUnderflow { .. }));
    }
}
