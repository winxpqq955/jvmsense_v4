//! Operand-stack depth computation.
//!
//! Replaces `ristretto_classfile`'s `MaxStack` implementation, which is wrong
//! for every invoke that returns a non-void value (see the module docs in
//! `bytecode/mod.rs`).
//!
//! The model is a simple linear scan: accumulate each instruction's stack
//! delta and record the high-water mark. Branch targets do not need to be
//! followed because the JVM's verifier requires the stack to be empty at
//! every merge point that this linear model would disagree on — for the code
//! *we* generate that is trivially true, and for code we merely rewrite we
//! carry the original value forward unless it is provably larger.

use crate::bytecode::error::BytecodeError;
use crate::bytecode::view::{InstructionView, Opcode};

/// Compute the maximum operand-stack depth reached by `instructions`.
///
/// # Errors
///
/// See [`BytecodeError`]. Underflow is reported rather than clamped, because a
/// too-small `max_stack` is a class that fails verification at load time —
/// far away from the code that got it wrong.
pub fn max_stack(instructions: &[InstructionView]) -> Result<u16, BytecodeError> {
    let mut depth: u32 = 0;
    let mut peak: u32 = 0;

    for instruction in instructions {
        let (pops, pushes) = effect(&instruction.opcode);
        let name = format!("{:?}", instruction.opcode);

        if pops > depth {
            return Err(BytecodeError::DepthUnderflow {
                opcode: name,
                needed: u16::try_from(pops).unwrap_or(u16::MAX),
                depth: u16::try_from(depth).unwrap_or(u16::MAX),
            });
        }
        depth = depth - pops + pushes;

        let limit = u32::from(u16::MAX);
        if depth > limit {
            return Err(BytecodeError::StackOverflow {
                depth,
                limit: u16::MAX,
            });
        }
        peak = peak.max(depth);
    }

    Ok(u16::try_from(peak).unwrap_or(u16::MAX))
}

/// `(slots popped, slots pushed)` for one opcode.
///
/// This is the table that ristretto gets wrong: for invokes it returns
/// `(params [+ receiver], 0)` regardless of the return type. Here the return
/// type contributes its real slot count.
fn effect(opcode: &Opcode) -> (u32, u32) {
    use Opcode as O;
    match opcode {
        // --- pushes ---
        O::AconstNull | O::IconstM1 | O::Iconst(_) | O::Fconst(_) => (0, 1),
        O::Lconst(_) | O::Dconst(_) => (0, 2),
        O::Bipush | O::Sipush | O::Ldc => (0, 1),
        O::Ldc2W => (0, 2),

        // --- local loads ---
        O::Iload(_) | O::Fload(_) | O::Aload(_) => (0, 1),
        O::Lload(_) | O::Dload(_) => (0, 2),

        // --- array loads ---
        O::Iaload | O::Faload | O::Aaload | O::Baload | O::Caload | O::Saload => (2, 1),
        O::Laload | O::Daload => (2, 2),

        // --- local stores ---
        O::Istore(_) | O::Fstore(_) | O::Astore(_) => (1, 0),
        O::Lstore(_) | O::Dstore(_) => (2, 0),

        // --- array stores ---
        O::Iastore | O::Fastore | O::Aastore | O::Bastore | O::Castore | O::Sastore => (3, 0),
        O::Lastore | O::Dastore => (4, 0),

        // --- stack manipulation ---
        O::Pop | O::Iinc(_) => (1, 0),
        O::Pop2 => (2, 0),
        O::Dup => (1, 2),
        // dup_x1: ..., v2, v1 -> ..., v1, v2, v1
        O::DupX1 => (2, 3),
        // dup_x2: ..., v3, v2, v1 -> ..., v1, v3, v2, v1
        O::DupX2 => (3, 4),
        // dup2: ..., v2, v1 -> ..., v2, v1, v2, v1
        O::Dup2 => (2, 4),
        // dup2_x1: ..., v3, v2, v1 -> ..., v2, v1, v3, v2, v1
        O::Dup2X1 => (3, 5),
        // dup2_x2: ..., v4, v3, v2, v1 -> ..., v2, v1, v4, v3, v2, v1
        O::Dup2X2 => (4, 6),
        O::Swap => (2, 2),

        // --- arithmetic ---
        O::IntBinaryOp | O::FloatBinaryOp => (2, 1),
        // long op: two 2-slot operands in, one 2-slot result out.
        // (`lshl`/`lshr`/`lushr` take a long and an *int*, so they are 3 in
        // and 2 out — modelled as a distinct width below if needed.)
        O::LongBinaryOp => (4, 2),
        O::DoubleBinaryOp => (4, 2),
        O::IntUnaryOp => (1, 1),
        O::LongUnaryOp | O::DoubleUnaryOp => (2, 2),

        // --- conversions ---
        O::ToWide => (1, 2),
        O::ToNarrow => (2, 1),
        O::SameWidth => (1, 1),

        // --- comparisons ---
        O::CompareWide => (4, 1),
        O::Branch => (1, 0),
        // if_acmp* pops two references
        O::BranchObjects => (2, 0),

        // --- returns ---
        O::Return => (0, 0),
        O::Ireturn | O::Freturn | O::Areturn => (1, 0),
        O::Lreturn | O::Dreturn => (2, 0),

        // --- fields ---
        // getstatic pushes the field's slot count
        O::Getstatic(d) => (0, u32::from(d.param_slots)),
        // putstatic pops it
        O::Putstatic(d) => (u32::from(d.param_slots), 0),
        // getfield: pops the receiver, pushes the field
        O::Getfield(d) => (1, u32::from(d.param_slots)),
        // putfield: pops the receiver and the value
        O::Putfield(d) => (1 + u32::from(d.param_slots), 0),

        // --- invokes: the case ristretto gets wrong ---
        // Each pops its parameters (plus a receiver for non-static forms) and
        // pushes its real return value.
        O::Invokestatic(d) => (u32::from(d.param_slots), u32::from(d.return_slots)),
        O::Invokevirtual(d) | O::Invokespecial(d) => {
            (1 + u32::from(d.param_slots), u32::from(d.return_slots))
        }
        O::Invokeinterface(d, _) => (1 + u32::from(d.param_slots), u32::from(d.return_slots)),
        // invokedynamic has no receiver; the descriptor is the call site's.
        O::Invokedynamic(d) => (u32::from(d.param_slots), u32::from(d.return_slots)),

        // --- object/array creation ---
        O::New => (0, 1),
        O::Newarray | O::Anewarray => (1, 1),
        O::Multianewarray(n) => (u32::from(*n), 1),
        O::Arraylength => (1, 1),

        // --- other ---
        O::Checkcast | O::Instanceof => (1, 1),
        O::MonitorEnter => (1, 0),
        O::Athrow => (1, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::view::{Descriptor, InvokeKind};

    fn d(s: &str) -> Descriptor {
        Descriptor::parse(s).expect("test descriptor")
    }

    #[test]
    fn invokestatic_returning_a_reference_pushes_one() {
        // ristretto says 0; the truth is 1.
        let instructions = vec![InstructionView::invoke(
            InvokeKind::Static,
            d("()Ljava/lang/String;"),
        )];
        assert_eq!(max_stack(&instructions).expect("compute"), 1);
    }

    #[test]
    fn invokestatic_returning_void_pushes_nothing() {
        let instructions = vec![InstructionView::invoke(InvokeKind::Static, d("()V"))];
        assert_eq!(max_stack(&instructions).expect("compute"), 0);
    }

    #[test]
    fn invokestatic_with_one_int_arg_and_int_return_is_balanced() {
        let instructions = vec![
            InstructionView::simple(Opcode::Iconst(1)),
            InstructionView::invoke(InvokeKind::Static, d("(I)I")),
        ];
        // push, pop one push one -> peak 1
        assert_eq!(max_stack(&instructions).expect("compute"), 1);
    }

    #[test]
    fn invokestatic_returning_a_long_pushes_two() {
        let instructions = vec![InstructionView::invoke(InvokeKind::Static, d("()J"))];
        assert_eq!(max_stack(&instructions).expect("compute"), 2);
    }

    #[test]
    fn invokevirtual_returning_a_reference_is_net_zero() {
        let instructions = vec![
            InstructionView::simple(Opcode::Aload(0)),
            InstructionView::invoke(InvokeKind::Virtual, d("()Ljava/lang/String;")),
        ];
        // aload -> 1; invokevirtual pops receiver (-1) and pushes result (+1)
        assert_eq!(max_stack(&instructions).expect("compute"), 1);
    }

    #[test]
    fn long_load_occupies_two_slots() {
        let instructions = vec![InstructionView::simple(Opcode::Lload(0))];
        assert_eq!(max_stack(&instructions).expect("compute"), 2);
    }

    #[test]
    fn getfield_pops_receiver_and_pushes_the_field_width() {
        let instructions = vec![
            InstructionView::simple(Opcode::Aload(0)),
            InstructionView::simple(Opcode::Getfield(d("J"))),
        ];
        // aload -> 1; getfield pops 1, pushes 2 -> 2
        assert_eq!(max_stack(&instructions).expect("compute"), 2);
    }

    #[test]
    fn underflow_is_an_error_not_a_clamp() {
        let instructions = vec![InstructionView::simple(Opcode::IntBinaryOp)];
        assert!(max_stack(&instructions).is_err());
    }

    #[test]
    fn dup2_of_a_reference_pair_doubles_it() {
        let instructions = vec![
            InstructionView::simple(Opcode::Iconst(0)),
            InstructionView::simple(Opcode::Iconst(1)),
            InstructionView::simple(Opcode::Dup2),
        ];
        assert_eq!(max_stack(&instructions).expect("compute"), 4);
    }
}
