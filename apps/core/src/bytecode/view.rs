//! A minimal, dependency-free view of the instruction stream.
//!
//! The point of this abstraction is that stack sizing must not inherit
//! `ristretto_classfile`'s bugs. Rather than pattern-match on that crate's
//! large `Instruction` enum (and depend on its descriptor parsing), callers
//! adapt into these two small types. A crate upgrade then cannot change our
//! answers without us noticing.

use crate::bytecode::error::BytecodeError;

/// The subset of opcodes that affect operand-stack depth or local slots.
///
/// Instructions with no stack effect and no local operand are not modelled;
/// callers map them to [`InstructionView::simple`] with any opcode that is
/// treated as a no-op, or simply omit them. Only opcodes whose effect this
/// module must know appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Opcode {
    // --- pushes of known width ---
    AconstNull,
    IconstM1,
    /// `iconst_0`..=`iconst_5`
    Iconst(u8),
    Lconst(u8),
    Fconst(u8),
    Dconst(u8),
    Bipush,
    Sipush,
    /// `ldc` / `ldc_w` — pushes one slot.
    Ldc,
    /// `ldc2_w` — pushes two slots.
    Ldc2W,

    // --- local variable loads ---
    Iload(u16),
    Lload(u16),
    Fload(u16),
    Dload(u16),
    Aload(u16),

    // --- array loads ---
    Iaload,
    Laload,
    Faload,
    Daload,
    Aaload,
    Baload,
    Caload,
    Saload,

    // --- local variable stores ---
    Istore(u16),
    Lstore(u16),
    Fstore(u16),
    Dstore(u16),
    Astore(u16),

    // --- array stores ---
    Iastore,
    Lastore,
    Fastore,
    Dastore,
    Aastore,
    Bastore,
    Castore,
    Sastore,

    // --- stack manipulation ---
    Pop,
    Pop2,
    Dup,
    DupX1,
    DupX2,
    Dup2,
    Dup2X1,
    Dup2X2,
    Swap,

    // --- arithmetic: each pops its operands and pushes its result ---
    /// `iadd`..`ixor`, `ishl`..`iushr` — 2 slots in, 1 out.
    IntBinaryOp,
    /// `ladd`..`lxor`, `lshl`..`lushr` — mixed widths, modelled explicitly.
    LongBinaryOp,
    /// `fadd`..`fxor` — 2 in, 1 out.
    FloatBinaryOp,
    /// `dadd`..`dxor` — 4 in, 2 out.
    DoubleBinaryOp,
    /// `ineg`, `fneg` — 1 in, 1 out (net zero).
    IntUnaryOp,
    /// `lneg` — 2 in, 2 out.
    LongUnaryOp,
    /// `dneg` — 2 in, 2 out.
    DoubleUnaryOp,
    /// `iinc` — no stack effect.
    Iinc(u16),

    // --- conversions ---
    /// `i2l`, `i2d`, `f2l`, `f2d` — widens to two slots.
    ToWide,
    /// `l2i`, `d2i`, `l2f`, `d2f` — narrows to one slot.
    ToNarrow,
    /// `i2f`, `f2i`, `i2b`, `i2c`, `i2s`, `l2d`, `d2l` — width preserved.
    SameWidth,

    // --- comparisons ---
    /// `lcmp`, `fcmpl`, `fcmpg`, `dcmpl`, `dcmpg` — 2 slots in, 1 out.
    CompareWide,
    /// `ifeq`..`ifle`, `if_icmp*`, `ifnull`, `ifnonnull`, `goto`, etc. —
    /// branches consume their operands; the target is irrelevant to depth.
    Branch,
    /// `if_acmp*` — two object refs in.
    BranchObjects,

    // --- returns ---
    Return,
    Ireturn,
    Lreturn,
    Freturn,
    Dreturn,
    Areturn,

    // --- field access, sized from the descriptor ---
    Getstatic(Descriptor),
    Putstatic(Descriptor),
    Getfield(Descriptor),
    Putfield(Descriptor),

    // --- invocation, sized from the descriptor ---
    Invokestatic(Descriptor),
    Invokevirtual(Descriptor),
    Invokespecial(Descriptor),
    Invokeinterface(Descriptor, u8),
    Invokedynamic(Descriptor),

    // --- object and array creation ---
    /// Pushes one reference.
    New,
    /// Pops a count, pushes an array reference.
    Newarray,
    Anewarray,
    /// `multianewarray` of the given dimensions.
    Multianewarray(u8),
    Arraylength,

    // --- other object instructions ---
    /// `checkcast` — 1 in, 1 out.
    Checkcast,
    /// `instanceof` — 1 in, 1 out.
    Instanceof,
    /// `monitorenter`, `monitorexit` — 1 in, 0 out.
    MonitorEnter,
    /// `athrow` — 1 in, 0 out (control does not continue).
    Athrow,
}

/// One instruction, paired with the descriptor-derived operand sizes the
/// opcode needs. Callers build these; see [`InstructionView::simple`] and the
/// `invoke_*` constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionView {
    pub opcode: Opcode,
}

/// Which invoke form an [`InstructionView::invoke`] call builds.
///
/// A separate tag avoids the Rust limitation that a tuple-variant name like
/// `Opcode::Invokestatic` is a constructor function, not a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeKind {
    Static,
    Virtual,
    Special,
    /// `invokeinterface`, with the argument-slot count byte the instruction
    /// encodes (the JVM ignores it, but it must be present in the class file).
    Interface(u8),
    Dynamic,
}

impl InstructionView {
    /// An instruction whose effect is fully described by its opcode.
    #[must_use]
    pub fn simple(opcode: Opcode) -> Self {
        Self { opcode }
    }

    /// An invoke instruction, sized from `descriptor`.
    #[must_use]
    pub fn invoke(kind: InvokeKind, descriptor: Descriptor) -> Self {
        let opcode = match kind {
            InvokeKind::Static => Opcode::Invokestatic(descriptor),
            InvokeKind::Virtual => Opcode::Invokevirtual(descriptor),
            InvokeKind::Special => Opcode::Invokespecial(descriptor),
            InvokeKind::Interface(count) => Opcode::Invokeinterface(descriptor, count),
            InvokeKind::Dynamic => Opcode::Invokedynamic(descriptor),
        };
        Self { opcode }
    }
}

/// A parsed JVM type descriptor: either a method descriptor or a field type.
///
/// Only the *slot counts* are retained. That is all stack sizing needs, and it
/// keeps this type free of any dependency on the crates we are working around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Slot count of the parameters. `long` and `double` count as two.
    pub param_slots: u16,
    /// Slot count of the return value: 0 for `void`, 1 usually, 2 for
    /// `long`/`double`.
    pub return_slots: u16,
    /// True when this is a *method* descriptor (contains `(...)`), false for a
    /// bare field type.
    pub is_method: bool,
}

impl Descriptor {
    /// Parse a descriptor, keeping only slot counts.
    ///
    /// # Errors
    ///
    /// Returns [`BytecodeError::MalformedDescriptor`] on any syntax error.
    /// Descriptors come from constant-pool strings that may be attacker
    /// influenced in a packaged app, so this never panics.
    pub fn parse(text: &str) -> Result<Self, BytecodeError> {
        let malformed = || BytecodeError::MalformedDescriptor {
            descriptor: text.to_string(),
        };

        if !text.contains('(') {
            // A bare field type, e.g. "I", "Ljava/lang/String;", "[J".
            let (slots, rest) = parse_field_type(text.as_bytes()).ok_or_else(malformed)?;
            if rest != text.len() {
                return Err(malformed());
            }
            return Ok(Self {
                param_slots: slots,
                return_slots: 0,
                is_method: false,
            });
        }

        let bytes = text.as_bytes();
        if bytes.first() != Some(&b'(') {
            return Err(malformed());
        }
        let mut i = 1usize;
        let mut param_slots = 0u16;
        while i < bytes.len() && bytes[i] != b')' {
            let (slots, next) = parse_field_type(&bytes[i..]).ok_or_else(malformed)?;
            param_slots = param_slots.checked_add(slots).ok_or_else(malformed)?;
            i += next;
        }
        if bytes.get(i) != Some(&b')') {
            return Err(malformed());
        }
        i += 1;

        let (return_slots, rest) = parse_return_type(&bytes[i..]).ok_or_else(malformed)?;
        if rest != bytes.len() - i {
            return Err(malformed());
        }

        Ok(Self {
            param_slots,
            return_slots,
            is_method: true,
        })
    }

    /// True when the described method returns nothing.
    #[must_use]
    pub fn returns_void(&self) -> bool {
        self.is_method && self.return_slots == 0
    }
}

/// Parse one field type from the front of `bytes`; returns (slots, consumed).
fn parse_field_type(bytes: &[u8]) -> Option<(u16, usize)> {
    match bytes.first()? {
        b'B' | b'C' | b'F' | b'I' | b'S' | b'Z' => Some((1, 1)),
        // long and double occupy two slots (JVMS §2.11.1)
        b'J' | b'D' => Some((2, 1)),
        b'L' => {
            let end = bytes.iter().position(|&b| b == b';')?;
            Some((1, end + 1))
        }
        b'[' => {
            let (_, inner) = parse_field_type(&bytes[1..])?;
            Some((1, 1 + inner))
        }
        _ => None,
    }
}

/// Parse a return type (or `V`); returns (slots, consumed).
fn parse_return_type(bytes: &[u8]) -> Option<(u16, usize)> {
    if bytes.first() == Some(&b'V') {
        return Some((0, 1));
    }
    parse_field_type(bytes)
}
