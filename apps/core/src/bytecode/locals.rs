//! Local-variable slot computation.
//!
//! Unlike `max_stack`, `ristretto_classfile`'s `max_locals` is plausibly
//! correct — it scans load/store indices without consulting descriptors. But
//! since we must own `max_stack` anyway, owning `max_locals` too means a
//! single implementation answers both, and the `long`/`double` two-slot rule
//! is applied in one place.
//!
//! `JVMS §2.6.1`: a `long` or `double` in a local variable occupies *two*
//! consecutive slots, and the index in `lload`/`dstore`/etc. refers to the
//! first of the pair. So an `lload n` requires `n + 2` slots to exist.

use crate::bytecode::error::BytecodeError;
use crate::bytecode::view::{Descriptor, InstructionView, Opcode};

/// Compute the number of local-variable slots the method needs.
///
/// This is the maximum of:
/// - the slots consumed by the incoming arguments (plus `this` when not
///   static), and
/// - the highest slot index touched by any local load/store, plus its width.
///
/// # Errors
///
/// Returns [`BytecodeError::StackOverflow`] if the required slot count exceeds
/// what the class file format can encode.
pub fn max_locals(
    instructions: &[InstructionView],
    descriptor: &Descriptor,
    is_static: bool,
) -> Result<u16, BytecodeError> {
    // Arguments occupy slots 0..param_slots, shifted by one for `this`.
    let mut required: u32 = u32::from(descriptor.param_slots) + u32::from(!is_static);

    for instruction in instructions {
        if let Some((index, width)) = local_access(&instruction.opcode) {
            let end = u32::from(index) + width;
            required = required.max(end);
        }
    }

    let limit = u32::from(u16::MAX);
    if required > limit {
        return Err(BytecodeError::StackOverflow {
            depth: required,
            limit: u16::MAX,
        });
    }
    Ok(u16::try_from(required).unwrap_or(u16::MAX))
}

/// `(first slot index, width in slots)` for instructions that address locals.
fn local_access(opcode: &Opcode) -> Option<(u16, u32)> {
    use Opcode as O;
    match opcode {
        O::Iload(i) | O::Fload(i) | O::Aload(i) | O::Istore(i) | O::Fstore(i) | O::Astore(i) => {
            Some((*i, 1))
        }
        O::Lload(i) | O::Dload(i) | O::Lstore(i) | O::Dstore(i) => Some((*i, 2)),
        // `iinc` writes one int slot but does not affect the operand stack.
        O::Iinc(i) => Some((*i, 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::view::InstructionView;

    fn d(s: &str) -> Descriptor {
        Descriptor::parse(s).expect("test descriptor")
    }

    #[test]
    fn static_no_args_needs_nothing() {
        let size = max_locals(&[], &d("()V"), true).expect("compute");
        assert_eq!(size, 0);
    }

    #[test]
    fn instance_no_args_needs_one_slot_for_this() {
        let size = max_locals(&[], &d("()V"), false).expect("compute");
        assert_eq!(size, 1);
    }

    #[test]
    fn long_parameter_takes_two_slots() {
        let size = max_locals(&[], &d("(J)V"), true).expect("compute");
        assert_eq!(size, 2);
    }

    #[test]
    fn mixed_parameters_sum_their_widths() {
        // int (1) + long (2) + String (1) = 4
        let size = max_locals(&[], &d("(IJLjava/lang/String;)V"), true).expect("compute");
        assert_eq!(size, 4);
    }

    #[test]
    fn instance_method_shifts_parameter_slots_by_one() {
        let size = max_locals(&[], &d("(I)V"), false).expect("compute");
        assert_eq!(size, 2); // this + int
    }

    #[test]
    fn local_access_beyond_parameters_extends_the_count() {
        let instructions = vec![InstructionView::simple(Opcode::Astore(5))];
        let size = max_locals(&instructions, &d("()V"), true).expect("compute");
        // astore 5 writes slot 5, so six slots (0..=5) must exist
        assert_eq!(size, 6);
    }

    #[test]
    fn two_slot_local_access_accounts_for_its_width() {
        let instructions = vec![InstructionView::simple(Opcode::Dstore(3))];
        let size = max_locals(&instructions, &d("()V"), true).expect("compute");
        // dstore 3 writes slots 3 and 4
        assert_eq!(size, 5);
    }

    #[test]
    fn local_access_inside_the_parameter_range_does_not_shrink_it() {
        let instructions = vec![InstructionView::simple(Opcode::Iload(0))];
        let size = max_locals(&instructions, &d("(IJ)V"), true).expect("compute");
        assert_eq!(size, 3, "parameters still reserve their slots");
    }
}
