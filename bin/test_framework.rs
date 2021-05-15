//! As part of this set of test cases, we define a mini IR and implement the
//! `Function` trait for it so that we can use the regalloc public interface.

use arbitrary::Arbitrary;
use regalloc::*;
use std::collections::HashSet;

use std::{borrow::Cow, fmt};

use crate::{
    parser::REFTYPE_START,
    validator::{validate, Context as ValidatorContext, RegRef},
};

use log::debug;

//=============================================================================
// Definition of: Label, RI (reg-or-immediate operands), AM (address modes),
// and Inst (instructions).  Also the get-regs and map-regs operations for
// them.  Destinations are on the left.

#[derive(Clone)]
pub enum Label {
    Unresolved { name: String },
    Resolved { name: String, bix: BlockIx },
}

impl Label {
    pub fn new_unresolved(name: String) -> Label {
        Label::Unresolved { name }
    }
    pub fn get_block_ix(&self) -> BlockIx {
        match self {
            Label::Resolved { name: _, bix } => *bix,
            Label::Unresolved { .. } => panic!("Label::getBlockIx: unresolved label!"),
        }
    }
    pub fn type_checks(&self, cx: &ValidatorContext) -> bool {
        match self {
            Label::Unresolved { .. } => false,
            Label::Resolved { bix, .. } => {
                let bix_u32: u32 = (*bix).into();
                bix_u32 < cx.num_blocks
            }
        }
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Label::Unresolved { name } => write!(fmt, "??:{}", &name),
            Label::Resolved { name, bix } => write!(fmt, "{:?}:{}", bix, name),
        }
    }
}

#[derive(Copy, Clone)]
pub enum RI {
    Reg { reg: Reg },
    Imm { imm: u32 },
}

#[allow(non_snake_case)]
pub fn RI_R(reg: Reg) -> RI {
    debug_assert!(reg.get_class() == RegClass::I32);
    RI::Reg { reg }
}

#[allow(non_snake_case)]
pub fn RI_I(imm: u32) -> RI {
    RI::Imm { imm }
}

impl fmt::Debug for RI {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RI::Reg { reg } => reg.fmt(fmt),
            RI::Imm { imm } => write!(fmt, "{}", imm),
        }
    }
}
impl RI {
    fn add_reg_reads_to(&self, collector: &mut RegUsageCollector) {
        match self {
            RI::Reg { reg } => collector.add_use(*reg),
            RI::Imm { .. } => {}
        }
    }
    fn apply_uses<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        match self {
            RI::Reg { ref mut reg } => {
                reg.apply_uses(mapper);
            }
            RI::Imm { .. } => {}
        }
    }
    fn type_checks(&self, cx: &mut ValidatorContext) -> bool {
        match self {
            RI::Reg { reg } => cx.check_reg_rc(reg, RegRef::Use, RegClass::I32),
            RI::Imm { .. } => true,
        }
    }
}

#[derive(Copy, Clone)]
pub enum AM {
    RI { base: Reg, offset: u32 },
    RR { base: Reg, offset: Reg },
}

#[allow(non_snake_case)]
pub fn AM_R(base: Reg) -> AM {
    debug_assert!(base.get_class() == RegClass::I32);
    AM::RI { base, offset: 0 }
}

#[allow(non_snake_case)]
pub fn AM_RI(base: Reg, offset: u32) -> AM {
    debug_assert!(base.get_class() == RegClass::I32);
    AM::RI { base, offset }
}

#[allow(non_snake_case)]
pub fn AM_RR(base: Reg, offset: Reg) -> AM {
    debug_assert!(base.get_class() == RegClass::I32);
    debug_assert!(offset.get_class() == RegClass::I32);
    AM::RR { base, offset }
}

impl fmt::Debug for AM {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AM::RI { base, offset } => write!(fmt, "[{:?}, {:?}]", base, offset),
            AM::RR { base, offset } => write!(fmt, "[{:?}, {:?}]", base, offset),
        }
    }
}

impl AM {
    fn add_reg_reads_to(&self, collector: &mut RegUsageCollector) {
        match self {
            AM::RI { base, .. } => collector.add_use(*base),
            AM::RR { base, offset } => {
                collector.add_use(*base);
                collector.add_use(*offset);
            }
        }
    }

    fn apply_uses<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        match self {
            AM::RI { ref mut base, .. } => {
                base.apply_uses(mapper);
            }
            AM::RR {
                ref mut base,
                ref mut offset,
            } => {
                base.apply_uses(mapper);
                offset.apply_uses(mapper);
            }
        }
    }

    fn type_checks(&self, cx: &mut ValidatorContext) -> bool {
        use RegClass::*;
        match self {
            AM::RI { base, .. } => cx.check_reg_rc(base, RegRef::Use, I32),
            AM::RR { base, offset } => {
                cx.check_reg_rc(base, RegRef::Use, I32) && cx.check_reg_rc(offset, RegRef::Use, I32)
            }
        }
    }
}

#[derive(Copy, Clone, Arbitrary)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Mod,
    Shr,
    And,
    CmpEQ,
    CmpLT,
    CmpLE,
    CmpGE,
    CmpGT,
}

impl fmt::Debug for BinOp {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "{}",
            match self {
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::Mul => "mul",
                BinOp::Mod => "mod",
                BinOp::Shr => "shr",
                BinOp::And => "and",
                BinOp::CmpEQ => "cmp_eq",
                BinOp::CmpLT => "cmp_lt",
                BinOp::CmpLE => "cmp_le",
                BinOp::CmpGE => "cmp_ge",
                BinOp::CmpGT => "cmp_gt",
            }
        )
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        (self as &dyn fmt::Debug).fmt(fmt)
    }
}

impl BinOp {
    pub fn calc(self, arg_left: u32, arg_right: u32) -> IResult<u32> {
        Ok(match self {
            BinOp::Add => u32::wrapping_add(arg_left, arg_right),
            BinOp::Sub => u32::wrapping_sub(arg_left, arg_right),
            BinOp::Mul => u32::wrapping_mul(arg_left, arg_right),
            BinOp::Mod => {
                if arg_right == 0 {
                    return Err("modulo by 0".into());
                }
                arg_left % arg_right
            }
            BinOp::Shr => arg_left >> (arg_right & 31),
            BinOp::And => arg_left & arg_right,
            BinOp::CmpEQ => {
                if arg_left == arg_right {
                    1
                } else {
                    0
                }
            }
            BinOp::CmpLT => {
                if arg_left < arg_right {
                    1
                } else {
                    0
                }
            }
            BinOp::CmpLE => {
                if arg_left <= arg_right {
                    1
                } else {
                    0
                }
            }
            BinOp::CmpGE => {
                if arg_left >= arg_right {
                    1
                } else {
                    0
                }
            }
            BinOp::CmpGT => {
                if arg_left > arg_right {
                    1
                } else {
                    0
                }
            }
        })
    }
}

#[derive(Copy, Clone, Arbitrary)]
pub enum BinOpF {
    FAdd,
    FSub,
    FMul,
    FDiv,
}

impl fmt::Debug for BinOpF {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "{}",
            match self {
                BinOpF::FAdd => "fadd".to_string(),
                BinOpF::FSub => "fsub".to_string(),
                BinOpF::FMul => "fmul".to_string(),
                BinOpF::FDiv => "fdiv".to_string(),
            }
        )
    }
}

impl fmt::Display for BinOpF {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        (self as &dyn fmt::Debug).fmt(fmt)
    }
}

impl BinOpF {
    pub fn calc(self, arg_left: f32, arg_right: f32) -> IResult<f32> {
        Ok(match self {
            BinOpF::FAdd => arg_left + arg_right,
            BinOpF::FSub => arg_left - arg_right,
            BinOpF::FMul => arg_left * arg_right,
            BinOpF::FDiv => {
                if arg_right == 0.0 {
                    return Err("division by zero".into());
                }
                arg_left / arg_right
            }
        })
    }
}

#[derive(Clone)]
pub enum Inst {
    NopZ {},
    Imm {
        dst: Reg,
        imm: u32,
    },
    ImmF {
        dst: Reg,
        imm: f32,
    },
    Copy {
        dst: Reg,
        src: Reg,
    },
    CopyF {
        dst: Reg,
        src: Reg,
    },
    BinOp {
        op: BinOp,
        dst: Reg,
        src_left: Reg,
        src_right: RI,
    },
    BinOpM {
        op: BinOp,
        dst: Reg,
        src_right: RI,
    }, // "mod" semantics for `dst`
    BinOpF {
        op: BinOpF,
        dst: Reg,
        src_left: Reg,
        src_right: Reg,
    },
    Load {
        dst: Reg,
        addr: AM,
    },
    LoadF {
        dst: Reg,
        addr: AM,
    },
    Store {
        addr: AM,
        src: Reg,
    },
    StoreF {
        addr: AM,
        src: Reg,
    },
    MakeRef {
        dst: Reg,
        src: Reg,
    },
    UseRef {
        dst: Reg,
        src: Reg,
    },
    Safepoint,
    Spill {
        dst: SpillSlot,
        src: RealReg,
    },
    SpillF {
        dst: SpillSlot,
        src: RealReg,
    },
    Reload {
        dst: RealReg,
        src: SpillSlot,
    },
    ReloadF {
        dst: RealReg,
        src: SpillSlot,
    },
    Goto {
        target: Label,
    },
    GotoCTF {
        cond: Reg,
        target_true: Label,
        target_false: Label,
    },
    PrintS {
        str: String,
    },
    PrintI {
        reg: Reg,
    },
    PrintF {
        reg: Reg,
    },
    Finish {
        reg: Option<Reg>,
    },
}

pub fn i_imm(dst: Reg, imm: u32) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::Imm { dst, imm }
}
pub fn i_immf(dst: Reg, imm: f32) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    Inst::ImmF { dst, imm }
}
pub fn i_copy(dst: Reg, src: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src.get_class() == RegClass::I32);
    Inst::Copy { dst, src }
}
pub fn i_copyf(dst: Reg, src: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    debug_assert!(src.get_class() == RegClass::F32);
    Inst::CopyF { dst, src }
}
// For BinOp variants see below

pub fn i_load(dst: Reg, addr: AM) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::Load { dst, addr }
}
pub fn i_loadf(dst: Reg, addr: AM) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    Inst::LoadF { dst, addr }
}
pub fn i_store(addr: AM, src: Reg) -> Inst {
    debug_assert!(src.get_class() == RegClass::I32);
    Inst::Store { addr, src }
}
pub fn i_storef(addr: AM, src: Reg) -> Inst {
    debug_assert!(src.get_class() == RegClass::F32);
    Inst::StoreF { addr, src }
}
fn i_spill(dst: SpillSlot, src: RealReg) -> Inst {
    debug_assert!(src.get_class() == RegClass::I32);
    Inst::Spill { dst, src }
}
fn i_spillf(dst: SpillSlot, src: RealReg) -> Inst {
    debug_assert!(src.get_class() == RegClass::F32);
    Inst::SpillF { dst, src }
}
fn i_reload(dst: RealReg, src: SpillSlot) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::Reload { dst, src }
}
fn i_reloadf(dst: RealReg, src: SpillSlot) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    Inst::ReloadF { dst, src }
}
pub fn i_goto<'a>(target: &'a str) -> Inst {
    Inst::Goto {
        target: Label::new_unresolved(target.to_string()),
    }
}
pub fn i_goto_ctf<'a>(cond: Reg, target_true: &'a str, target_false: &'a str) -> Inst {
    debug_assert!(cond.get_class() == RegClass::I32);
    Inst::GotoCTF {
        cond,
        target_true: Label::new_unresolved(target_true.to_string()),
        target_false: Label::new_unresolved(target_false.to_string()),
    }
}
pub fn i_print_s<'a>(str: &'a str) -> Inst {
    Inst::PrintS {
        str: str.to_string(),
    }
}
pub fn i_print_i(reg: Reg) -> Inst {
    debug_assert!(reg.get_class() == RegClass::I32);
    Inst::PrintI { reg }
}
pub fn i_print_f(reg: Reg) -> Inst {
    debug_assert!(reg.get_class() == RegClass::F32);
    Inst::PrintF { reg }
}
pub fn i_finish(reg: Option<Reg>) -> Inst {
    Inst::Finish { reg }
}

pub fn i_add(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::Add,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_sub(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::Sub,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_mul(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::Mul,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_mod(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::Mod,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_shr(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::Shr,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_and(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::And,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_cmp_eq(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::CmpEQ,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_cmp_lt(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::CmpLT,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_cmp_le(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::CmpLE,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_cmp_ge(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::CmpGE,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_cmp_gt(dst: Reg, src_left: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    debug_assert!(src_left.get_class() == RegClass::I32);
    Inst::BinOp {
        op: BinOp::CmpGT,
        dst,
        src_left,
        src_right,
    }
}

// 2-operand versions of i_add and i_sub, for experimentation
pub fn i_addm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::Add,
        dst,
        src_right,
    }
}
pub fn i_andm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::And,
        dst,
        src_right,
    }
}
pub fn i_modm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::Mod,
        dst,
        src_right,
    }
}
pub fn i_mulm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::Mul,
        dst,
        src_right,
    }
}
pub fn i_shrm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::Shr,
        dst,
        src_right,
    }
}
pub fn i_subm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::Sub,
        dst,
        src_right,
    }
}
pub fn i_cmp_eqm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::CmpEQ,
        dst,
        src_right,
    }
}
pub fn i_cmp_gem(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::CmpGE,
        dst,
        src_right,
    }
}
pub fn i_cmp_gtm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::CmpGT,
        dst,
        src_right,
    }
}
pub fn i_cmp_lem(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::CmpLE,
        dst,
        src_right,
    }
}
pub fn i_cmp_ltm(dst: Reg, src_right: RI) -> Inst {
    debug_assert!(dst.get_class() == RegClass::I32);
    Inst::BinOpM {
        op: BinOp::CmpLT,
        dst,
        src_right,
    }
}

pub fn i_fadd(dst: Reg, src_left: Reg, src_right: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    debug_assert!(src_left.get_class() == RegClass::F32);
    debug_assert!(src_right.get_class() == RegClass::F32);
    Inst::BinOpF {
        op: BinOpF::FAdd,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_fsub(dst: Reg, src_left: Reg, src_right: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    debug_assert!(src_left.get_class() == RegClass::F32);
    debug_assert!(src_right.get_class() == RegClass::F32);
    Inst::BinOpF {
        op: BinOpF::FSub,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_fmul(dst: Reg, src_left: Reg, src_right: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    debug_assert!(src_left.get_class() == RegClass::F32);
    debug_assert!(src_right.get_class() == RegClass::F32);
    Inst::BinOpF {
        op: BinOpF::FMul,
        dst,
        src_left,
        src_right,
    }
}
pub fn i_fdiv(dst: Reg, src_left: Reg, src_right: Reg) -> Inst {
    debug_assert!(dst.get_class() == RegClass::F32);
    debug_assert!(src_left.get_class() == RegClass::F32);
    debug_assert!(src_right.get_class() == RegClass::F32);
    Inst::BinOpF {
        op: BinOpF::FDiv,
        dst,
        src_left,
        src_right,
    }
}

impl fmt::Debug for Inst {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fn ljustify(s: String, w: usize) -> String {
            if s.len() >= w {
                s
            } else {
                // BEGIN hack
                let mut need = w - s.len();
                if need > 5 {
                    need = 5;
                }
                let extra = [" ", "  ", "   ", "    ", "     "][need - 1];
                // END hack
                s + &extra.to_string()
            }
        }

        match self {
            Inst::NopZ {} => write!(fmt, "nopz"),
            Inst::Imm { dst, imm } => write!(fmt, "imm     {:?}, {:?}", dst, imm),
            Inst::ImmF { dst, imm } => write!(fmt, "immf    {:?}, {:?}", dst, imm),
            Inst::Copy { dst, src } => write!(fmt, "copy    {:?}, {:?}", dst, src),
            Inst::CopyF { dst, src } => write!(fmt, "copyf   {:?}, {:?}", dst, src),
            Inst::BinOp {
                op,
                dst,
                src_left,
                src_right,
            } => write!(
                fmt,
                "{} {:?}, {:?}, {:?}",
                ljustify(op.to_string(), 7),
                dst,
                src_left,
                src_right
            ),
            Inst::BinOpM { op, dst, src_right } => write!(
                fmt,
                "{} {:?}, {:?}",
                ljustify(op.to_string() + &"m".to_string(), 7),
                dst,
                src_right
            ),
            Inst::BinOpF {
                op,
                dst,
                src_left,
                src_right,
            } => write!(
                fmt,
                "{} {:?}, {:?}, {:?}",
                ljustify(op.to_string(), 7),
                dst,
                src_left,
                src_right
            ),
            Inst::Load { dst, addr } => write!(fmt, "load    {:?}, {:?}", dst, addr),
            Inst::LoadF { dst, addr } => write!(fmt, "loadf   {:?}, {:?}", dst, addr),
            Inst::Store { addr, src } => write!(fmt, "store   {:?}, {:?}", addr, src),
            Inst::StoreF { addr, src } => write!(fmt, "storef  {:?}, {:?}", addr, src),
            Inst::MakeRef { dst, src } => write!(fmt, "makeref {:?}, {:?}", dst, src),
            Inst::UseRef { dst, src } => write!(fmt, "useref  {:?}, {:?}", dst, src),
            Inst::Safepoint => write!(fmt, "safepoint"),
            Inst::Spill { dst, src } => write!(fmt, "SPILL   {:?}, {:?}", dst, src),
            Inst::SpillF { dst, src } => write!(fmt, "SPILLF  {:?}, {:?}", dst, src),
            Inst::Reload { dst, src } => write!(fmt, "RELOAD  {:?}, {:?}", dst, src),
            Inst::ReloadF { dst, src } => write!(fmt, "RELOAD  {:?}, {:?}", dst, src),
            Inst::Goto { target } => write!(fmt, "goto    {:?}", target),
            Inst::GotoCTF {
                cond,
                target_true,
                target_false,
            } => write!(
                fmt,
                "if_then_else {:?}, {:?}, {:?}",
                cond, target_true, target_false
            ),
            Inst::PrintS { str } => {
                let mut res = "prints  '".to_string();
                for c in str.chars() {
                    res += &(if c == '\n' {
                        "\\n".to_string()
                    } else {
                        c.to_string()
                    });
                }
                write!(fmt, "{}'", res)
            }
            Inst::PrintI { reg } => write!(fmt, "printi  {:?}", reg),
            Inst::PrintF { reg } => write!(fmt, "printf  {:?}", reg),
            Inst::Finish { reg } => {
                if let Some(reg) = reg {
                    write!(fmt, "finish  {:?}", reg)
                } else {
                    write!(fmt, "finish")
                }
            }
        }
    }
}

impl Inst {
    // Returns a vector of BlockIxs, being those that this insn might jump to.
    // This might contain duplicates (although it would be pretty strange if
    // it did). This function should not be applied to non-control-flow
    // instructions.  The labels are assumed all to be "resolved".
    pub fn get_targets(&self) -> Vec<BlockIx> {
        match self {
            Inst::Goto { target } => vec![target.get_block_ix()],
            Inst::GotoCTF {
                cond: _,
                target_true,
                target_false,
            } => vec![target_true.get_block_ix(), target_false.get_block_ix()],
            Inst::Finish { reg: _ } => vec![],
            _other => panic!("Inst::getTargets: incorrectly applied to: {:?}", self),
        }
    }

    // Returns three sets of regs, (def, mod, use), being those def'd (written),
    // those mod'd (modified) and those use'd (read) by the instruction,
    // respectively.
    //
    // Be careful here.  If an instruction really modifies a register -- as is
    // typical for x86 -- that register needs to be in the `mod` set, and not in
    // the `def` and `use` sets.  *Any* mistake in describing register uses here
    // will almost certainly lead to incorrect register allocations.
    //
    // Also the following must hold: the union of `def` and `use` must be
    // disjoint from `mod`.
    pub fn get_reg_usage(&self, collector: &mut RegUsageCollector) {
        match self {
            Inst::NopZ {} => {}
            Inst::Imm { dst, imm: _ } => {
                collector.add_def(Writable::from_reg(*dst));
            }
            Inst::ImmF { dst, imm: _ } => {
                collector.add_def(Writable::from_reg(*dst));
            }
            Inst::Copy { dst, src } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src);
            }
            Inst::CopyF { dst, src } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src);
            }
            Inst::BinOp {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src_left);
                src_right.add_reg_reads_to(collector);
            }
            Inst::BinOpM {
                op: _,
                dst,
                src_right,
            } => {
                collector.add_mod(Writable::from_reg(*dst));
                src_right.add_reg_reads_to(collector);
            }
            Inst::BinOpF {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src_left);
                collector.add_use(*src_right);
            }
            Inst::Store { addr, src } => {
                addr.add_reg_reads_to(collector);
                collector.add_use(*src);
            }
            Inst::StoreF { addr, src } => {
                addr.add_reg_reads_to(collector);
                collector.add_use(*src);
            }
            Inst::MakeRef { dst, src } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src);
            }
            Inst::UseRef { dst, src } => {
                collector.add_def(Writable::from_reg(*dst));
                collector.add_use(*src);
            }
            Inst::Safepoint => {}
            Inst::Load { dst, addr } => {
                collector.add_def(Writable::from_reg(*dst));
                addr.add_reg_reads_to(collector);
            }
            Inst::LoadF { dst, addr } => {
                collector.add_def(Writable::from_reg(*dst));
                addr.add_reg_reads_to(collector);
            }
            Inst::Goto { .. } => {}
            Inst::GotoCTF {
                cond,
                target_true: _,
                target_false: _,
            } => {
                collector.add_use(*cond);
            }
            Inst::PrintS { .. } => {}
            Inst::PrintI { reg } => {
                collector.add_use(*reg);
            }
            Inst::PrintF { reg } => {
                collector.add_use(*reg);
            }
            Inst::Finish { reg } => {
                if let Some(reg) = reg {
                    collector.add_use(*reg);
                }
            }
            // Spill and Reload are seen here during the final pass over insts that
            // computes clobbered regs.
            Inst::Spill { src, .. } | Inst::SpillF { src, .. } => {
                collector.add_use(src.to_reg());
            }
            Inst::Reload { dst, .. } | Inst::ReloadF { dst, .. } => {
                collector.add_def(Writable::from_reg(dst.to_reg()));
            }
        }
    }

    /// Apply the specified VirtualReg->RealReg mappings to the instruction,
    pub fn map_regs<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        match self {
            Inst::NopZ {} => {}
            Inst::Imm { dst, imm: _ } => {
                dst.apply_defs(mapper);
            }
            Inst::ImmF { dst, imm: _ } => {
                dst.apply_defs(mapper);
            }
            Inst::Copy { dst, src } => {
                dst.apply_defs(mapper);
                src.apply_uses(mapper);
            }
            Inst::CopyF { dst, src } => {
                dst.apply_defs(mapper);
                src.apply_uses(mapper);
            }
            Inst::BinOp {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                dst.apply_defs(mapper);
                src_left.apply_uses(mapper);
                src_right.apply_uses(mapper);
            }
            Inst::BinOpM {
                op: _,
                dst,
                src_right,
            } => {
                dst.apply_mods(mapper);
                src_right.apply_uses(mapper);
            }
            Inst::BinOpF {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                dst.apply_defs(mapper);
                src_left.apply_uses(mapper);
                src_right.apply_uses(mapper);
            }
            Inst::MakeRef { dst, src } => {
                dst.apply_defs(mapper);
                src.apply_uses(mapper);
            }
            Inst::UseRef { dst, src } => {
                dst.apply_defs(mapper);
                src.apply_uses(mapper);
            }
            Inst::Safepoint => {}
            Inst::Store { addr, src } => {
                addr.apply_uses(mapper);
                src.apply_uses(mapper);
            }
            Inst::StoreF { addr, src } => {
                addr.apply_uses(mapper);
                src.apply_uses(mapper);
            }
            Inst::Load { dst, addr } => {
                dst.apply_defs(mapper);
                addr.apply_uses(mapper);
            }
            Inst::LoadF { dst, addr } => {
                dst.apply_defs(mapper);
                addr.apply_uses(mapper);
            }
            Inst::Goto { .. } => {}
            Inst::GotoCTF {
                cond,
                target_true: _,
                target_false: _,
            } => {
                cond.apply_uses(mapper);
            }
            Inst::PrintS { .. } => {}
            Inst::PrintI { reg } => {
                reg.apply_uses(mapper);
            }
            Inst::PrintF { reg } => {
                reg.apply_uses(mapper);
            }
            Inst::Finish { reg } => {
                if let Some(reg) = reg {
                    reg.apply_uses(mapper);
                }
            }
            Inst::Spill { .. }
            | Inst::SpillF { .. }
            | Inst::Reload { .. }
            | Inst::ReloadF { .. } => {
                unreachable!("no need to fill in instructions inserted by regalloc")
            }
        }
    }

    pub fn is_control_flow(&self) -> bool {
        match self {
            Inst::Goto { .. } | Inst::GotoCTF { .. } | Inst::Finish { reg: _ } => true,
            _ => false,
        }
    }

    /// Is this instruction a user instruction, or is it internal to regalloc?
    pub fn is_user(&self) -> bool {
        match self {
            Inst::Spill { .. }
            | Inst::SpillF { .. }
            | Inst::Reload { .. }
            | Inst::ReloadF { .. } => false,
            _ => true,
        }
    }

    pub fn type_checks(&self, cx: &mut ValidatorContext) -> bool {
        use RegClass::*;
        // Always check uses before defs.
        match self {
            Inst::NopZ {} => true,
            Inst::Imm { dst, imm: _ } => cx.check_reg_rc(dst, RegRef::Def, I32),
            Inst::ImmF { dst, imm: _ } => cx.check_reg_rc(dst, RegRef::Def, F32),
            // MakeRef and UseRef are like copy instructions; the typecheck only
            // reasons about register classes, not reffyness, so the
            // distinctions between the three are irrelevant here.
            Inst::Copy { dst, src } | Inst::MakeRef { dst, src } | Inst::UseRef { dst, src } => {
                cx.check_reg_rc(src, RegRef::Use, I32) && cx.check_reg_rc(dst, RegRef::Def, I32)
            }
            Inst::CopyF { dst, src } => {
                cx.check_reg_rc(src, RegRef::Use, F32) && cx.check_reg_rc(dst, RegRef::Def, F32)
            }
            Inst::Load { dst, addr } => {
                addr.type_checks(cx) && cx.check_reg_rc(dst, RegRef::Def, I32)
            }
            Inst::LoadF { dst, addr } => {
                addr.type_checks(cx) && cx.check_reg_rc(dst, RegRef::Def, F32)
            }
            Inst::Store { addr, src } => {
                cx.check_reg_rc(src, RegRef::Use, I32) && addr.type_checks(cx)
            }
            Inst::StoreF { addr, src } => {
                cx.check_reg_rc(src, RegRef::Use, F32) && addr.type_checks(cx)
            }
            Inst::Goto { target } => target.type_checks(cx),
            Inst::GotoCTF {
                cond,
                target_true,
                target_false,
            } => {
                cx.check_reg_rc(cond, RegRef::Use, I32)
                    && target_true.type_checks(cx)
                    && target_false.type_checks(cx)
            }
            Inst::PrintS { .. } => true,
            Inst::PrintI { reg } => cx.check_reg_rc(reg, RegRef::Use, I32),
            Inst::PrintF { reg } => cx.check_reg_rc(reg, RegRef::Use, F32),
            Inst::Finish { reg } => reg.map_or(true, |reg| cx.check_reg(reg, RegRef::Use)),
            Inst::BinOp {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                cx.check_reg_rc(src_left, RegRef::Use, I32)
                    && src_right.type_checks(cx)
                    && cx.check_reg_rc(dst, RegRef::Def, I32)
            }
            Inst::BinOpM {
                op: _,
                dst,
                src_right,
            } => {
                cx.check_reg_rc(dst, RegRef::Use, I32)
                    && src_right.type_checks(cx)
                    && cx.check_reg_rc(dst, RegRef::Def, I32)
            }
            Inst::BinOpF {
                op: _,
                dst,
                src_left,
                src_right,
            } => {
                cx.check_reg_rc(src_left, RegRef::Use, F32)
                    && cx.check_reg_rc(src_right, RegRef::Use, F32)
                    && cx.check_reg_rc(dst, RegRef::Def, F32)
            }
            Inst::Safepoint => true,

            // These are not user instructions.
            Inst::Spill { .. }
            | Inst::SpillF { .. }
            | Inst::Reload { .. }
            | Inst::ReloadF { .. } => {
                panic!("unexpected spill/spillf/reload/reloadf in type_checks")
            }
        }
    }
}

//=============================================================================
// The interpreter

#[derive(Copy, Clone)]
pub enum Value {
    U32(u32),
    F32(f32),
    Ref(u32),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Value::U32(x) => match other {
                Value::U32(y) => x == y,
                _ => false,
            },

            Value::F32(x) => match other {
                Value::F32(y) => (x.is_nan() && y.is_nan()) || x == y,
                _ => false,
            },

            Value::Ref(x) => match other {
                Value::Ref(y) => x == y,
                _ => false,
            },
        }
    }
}

impl Value {
    fn to_u32(self) -> u32 {
        match self {
            Value::U32(n) => n,
            Value::F32(_) => panic!("Value::toU32: this is a F32"),
            Value::Ref(_) => panic!("Value::toU32: this is a ref"),
        }
    }
    fn to_f32(self) -> f32 {
        match self {
            Value::U32(_) => panic!("Value::toF32: this is a U32"),
            Value::Ref(_) => panic!("Value::toF32: this is a ref"),
            Value::F32(n) => n,
        }
    }
    fn to_ref(self) -> u32 {
        match self {
            Value::Ref(n) => n,
            _ => panic!("Value::to_ref: this is not a ref"),
        }
    }
    fn cast_to_u32(self) -> u32 {
        match self {
            Value::U32(n) => n,
            Value::F32(f) => f as u32,
            Value::Ref(n) => n,
        }
    }
    fn cast_to_f32(self) -> f32 {
        match self {
            Value::U32(n) => n as f32,
            Value::F32(f) => f,
            Value::Ref(n) => n as f32,
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::U32(n) => write!(fmt, "{}", n),
            Value::F32(n) => write!(fmt, "{}", n),
            Value::Ref(n) => write!(fmt, "@{}", n),
        }
    }
}

#[derive(PartialEq)]
pub enum RunStage {
    BeforeRegalloc,
    AfterRegalloc,
}

struct IState<'a> {
    func: &'a Func,
    nia: InstIx,               // Program counter ("next instruction address")
    vregs: Vec<Option<Value>>, // unlimited
    rregs: Vec<Option<Value>>, // [0 .. maxRealRegs)
    mem: Vec<Option<Value>>,   // [0 .. maxMem)
    slots: Vec<Option<Value>>, // [0..] Spill slots, no upper limit
    num_insts: usize,          // Stats: number of insns executed
    num_spills: usize,         // Stats: .. of which are spills
    num_reloads: usize,        // Stats: .. of which are reloads
    run_stage: RunStage,
    ret_value: Option<Value>,
    stdout: String, // Everything that's printed out.
}

type IResult<T> = Result<T, String>;

impl<'a> IState<'a> {
    fn new(func: &'a Func, max_real_regs: usize, max_mem: usize, run_stage: RunStage) -> Self {
        let mut state = IState {
            func,
            nia: func.blocks[func
                .entry
                .as_ref()
                .expect("missing entry block")
                .get_block_ix()
                .get() as usize]
                .start,
            vregs: Vec::new(),
            rregs: Vec::new(),
            mem: Vec::new(),
            slots: Vec::new(),
            num_insts: 0,
            num_spills: 0,
            num_reloads: 0,
            run_stage,
            ret_value: None,
            stdout: String::new(),
        };
        state.rregs.resize(max_real_regs, None);
        state.mem.resize(max_mem, None);
        state
    }

    fn get_real_reg(&self, rreg: RealReg) -> IResult<Value> {
        // No automatic resizing.  If the rreg doesn't exist, just fail.
        match self.rregs.get(rreg.get_index()) {
            None => panic!("IState::get_real_reg: invalid rreg {:?}", rreg),
            Some(None) => {
                return Err(format!(
                    "IState::get_real_reg: read of uninit rreg {:?} at nia {:?}",
                    rreg, self.nia
                ))?
            }
            Some(Some(val)) => Ok(*val),
        }
    }

    fn set_real_reg(&mut self, rreg: RealReg, val: Value) {
        // No automatic resizing.  If the rreg doesn't exist, just fail.
        match self.rregs.get_mut(rreg.get_index()) {
            None => panic!("IState::setRealReg: invalid rreg {:?}", rreg),
            Some(val_p) => *val_p = Some(val),
        }
    }

    fn get_virtual_reg(&self, vreg: VirtualReg) -> IResult<Value> {
        debug_assert!(
            self.run_stage != RunStage::AfterRegalloc,
            "trying to get a vreg after regalloc"
        );
        // The vector might be too small.  But in that case we'd be
        // reading the vreg uninitialised anyway, so just complain.
        match self.vregs.get(vreg.get_index()) {
            None | Some(None) => {
                // entry either present or absent, but has never been written in both
                // cases.
                return Err(format!(
                    "IState::get_virtual_reg: read of uninit vreg {:?}",
                    vreg
                ))?;
            }
            Some(Some(val)) => Ok(*val),
        }
    }

    fn set_virtual_reg(&mut self, vreg: VirtualReg, val: Value) {
        debug_assert!(
            self.run_stage != RunStage::AfterRegalloc,
            "trying to set a vreg after regalloc"
        );
        // Auto-resize the vector if necessary
        let ix = vreg.get_index();
        if ix >= self.vregs.len() {
            self.vregs.resize(ix + 1, None);
        }
        debug_assert!(ix < self.vregs.len());
        self.vregs[ix] = Some(val);
    }

    fn get_spill_slot(&self, slot: SpillSlot) -> Value {
        // The vector might be too small.  But in that case we'd be
        // reading the slot uninitialised anyway, so just complain.
        match self.slots.get(slot.get_usize()) {
            None |          // indexing error
            Some(None) =>   // entry present, but has never been written
                panic!("IState::getSpillSlot: read of uninit slot # {}",
                       slot.get()),
            Some(Some(val))
                => *val
        }
    }

    fn set_spill_slot_u32(&mut self, slot: SpillSlot, val: Value) {
        // Auto-resize the vector if necessary
        let ix = slot.get_usize();
        if ix >= self.slots.len() {
            self.slots.resize(ix + 1, None);
        }
        debug_assert!(ix < self.slots.len());
        debug_assert!(matches!(val, Value::Ref(_) | Value::U32(_)));
        self.slots[ix] = Some(val);
    }

    fn set_spill_slot_f32(&mut self, slot: SpillSlot, val: f32) {
        // Auto-resize the vector if necessary
        let ix = slot.get_usize();
        if ix >= self.slots.len() {
            self.slots.resize(ix + 1, None);
        }
        debug_assert!(ix < self.slots.len());
        self.slots[ix] = Some(Value::F32(val));
    }

    fn get_reg(&self, reg: Reg) -> IResult<Value> {
        if reg.is_virtual() {
            self.get_virtual_reg(reg.to_virtual_reg())
        } else {
            self.get_real_reg(reg.to_real_reg())
        }
    }

    fn set_reg_u32(&mut self, reg: Reg, val: u32) {
        self.set_reg(reg, Value::U32(val))
    }

    fn set_reg_f32(&mut self, reg: Reg, val: f32) {
        self.set_reg(reg, Value::F32(val))
    }

    fn set_reg_ref(&mut self, reg: Reg, val: u32) {
        self.set_reg(reg, Value::Ref(val))
    }

    fn set_reg(&mut self, reg: Reg, val: Value) {
        if reg.is_virtual() {
            self.set_virtual_reg(reg.to_virtual_reg(), val);
        } else {
            self.set_real_reg(reg.to_real_reg(), val);
        }
    }

    fn get_mem(&self, addr: u32) -> IResult<Value> {
        // No auto resizing of the memory.
        Ok(match self.mem.get(addr as usize) {
            None => return Err(format!("IState::getMem: invalid addr {}", addr))?,
            Some(None) => {
                return Err(format!(
                    "IState::getMem: read of uninit mem at addr {}",
                    addr
                ))?
            }
            Some(Some(val)) => *val,
        })
    }

    fn set_mem_u32(&mut self, addr: u32, val: u32) -> IResult<()> {
        // No auto resizing of the memory
        match self.mem.get_mut(addr as usize) {
            None => return Err(format!("IState::set_mem_u32: invalid addr {}", addr))?,
            Some(val_p) => *val_p = Some(Value::U32(val)),
        }
        Ok(())
    }

    fn set_mem_f32(&mut self, addr: u32, val: f32) -> IResult<()> {
        // No auto resizing of the memory
        match self.mem.get_mut(addr as usize) {
            None => return Err(format!("IState::set_mem_f32: invalid addr {}", addr))?,
            Some(val_p) => *val_p = Some(Value::F32(val)),
        }
        Ok(())
    }

    #[allow(non_snake_case)]
    fn get_RI(&self, ri: &RI) -> IResult<u32> {
        Ok(match ri {
            RI::Reg { reg } => self.get_reg(*reg)?.to_u32(),
            RI::Imm { imm } => *imm,
        })
    }

    #[allow(non_snake_case)]
    fn get_AM(&self, am: &AM) -> IResult<u32> {
        // Base + offset can overflow and wraparound, because why not?
        Ok(match am {
            AM::RI { base, offset } => self.get_reg(*base)?.to_u32().wrapping_add(*offset),
            AM::RR { base, offset } => self
                .get_reg(*base)?
                .to_u32()
                .wrapping_add(self.get_reg(*offset)?.to_u32()),
        })
    }

    // Move the interpreter one step forward
    fn step(&mut self) -> IResult<bool> {
        let mut done = false;

        let iix = self.nia;
        self.nia = iix.plus(1);
        self.num_insts += 1;

        let insn = &self.func.insns[iix.get() as usize];
        match insn {
            Inst::NopZ {} => {
                self.num_insts -= 1;
            }
            Inst::Imm { dst, imm } => self.set_reg_u32(*dst, *imm),
            Inst::ImmF { dst, imm } => self.set_reg_f32(*dst, *imm),
            Inst::Copy { dst, src } => self.set_reg(*dst, self.get_reg(*src)?),
            Inst::CopyF { dst, src } => self.set_reg_f32(*dst, self.get_reg(*src)?.to_f32()),
            Inst::BinOp {
                op,
                dst,
                src_left,
                src_right,
            } => {
                let src_left_v = self.get_reg(*src_left)?.to_u32();
                let src_right_v = self.get_RI(src_right)?;
                let dst_v = op.calc(src_left_v, src_right_v)?;
                self.set_reg_u32(*dst, dst_v);
            }
            Inst::BinOpM { op, dst, src_right } => {
                let mut dst_v = self.get_reg(*dst)?.to_u32();
                let src_right_v = self.get_RI(src_right)?;
                dst_v = op.calc(dst_v, src_right_v)?;
                self.set_reg_u32(*dst, dst_v);
            }
            Inst::BinOpF {
                op,
                dst,
                src_left,
                src_right,
            } => {
                let src_left_v = self.get_reg(*src_left)?.to_f32();
                let src_right_v = self.get_reg(*src_right)?.to_f32();
                let dst_v = op.calc(src_left_v, src_right_v)?;
                self.set_reg_f32(*dst, dst_v);
            }
            Inst::MakeRef { dst, src } => {
                let src_v = self.get_reg(*src)?.to_u32();
                self.set_reg_ref(*dst, src_v);
            }
            Inst::UseRef { dst, src } => {
                let src_v = self.get_reg(*src)?.to_ref();
                self.set_reg_u32(*dst, src_v);
            }
            Inst::Safepoint => {}
            Inst::Load { dst, addr } => {
                let addr_v = self.get_AM(addr)?;
                let dst_v = self.get_mem(addr_v)?.cast_to_u32();
                self.set_reg_u32(*dst, dst_v);
            }
            Inst::LoadF { dst, addr } => {
                let addr_v = self.get_AM(addr)?;
                let dst_v = self.get_mem(addr_v)?.cast_to_f32();
                self.set_reg_f32(*dst, dst_v);
            }
            Inst::Store { addr, src } => {
                let addr_v = self.get_AM(addr)?;
                let src_v = self.get_reg(*src)?.to_u32();
                self.set_mem_u32(addr_v, src_v)?;
            }
            Inst::StoreF { addr, src } => {
                let addr_v = self.get_AM(addr)?;
                let src_v = self.get_reg(*src)?.to_f32();
                self.set_mem_f32(addr_v, src_v)?;
            }
            Inst::Spill { dst, src } => {
                let src_v = self.get_real_reg(*src)?;
                self.set_spill_slot_u32(*dst, src_v);
                self.num_spills += 1;
            }
            Inst::SpillF { dst, src } => {
                let src_v = self.get_real_reg(*src)?.to_f32();
                self.set_spill_slot_f32(*dst, src_v);
                self.num_spills += 1;
            }
            Inst::Reload { dst, src } => {
                let src_v = self.get_spill_slot(*src);
                match src_v {
                    Value::Ref(n) => self.set_reg_ref(dst.to_reg(), n),
                    Value::U32(n) => self.set_reg_u32(dst.to_reg(), n),
                    _ => panic!("must be a ref or u32"),
                }
                self.num_reloads += 1;
            }
            Inst::ReloadF { dst, src } => {
                let src_v = self.get_spill_slot(*src).to_f32();
                self.set_reg_f32(dst.to_reg(), src_v);
                self.num_reloads += 1;
            }
            Inst::Goto { target } => {
                self.nia = self.func.blocks[target.get_block_ix().get() as usize].start
            }
            Inst::GotoCTF {
                cond,
                target_true,
                target_false,
            } => {
                let target = if self.get_reg(*cond)?.to_u32() != 0 {
                    target_true
                } else {
                    target_false
                };
                self.nia = self.func.blocks[target.get_block_ix().get() as usize].start;
            }
            Inst::PrintS { str } => {
                self.stdout += str;
                print!("{}", str);
            }
            Inst::PrintI { reg } => {
                let str = format!("{:?}", self.get_reg(*reg)?.to_u32());
                print!("{}", str);
                self.stdout += &str;
            }
            Inst::PrintF { reg } => {
                let str = format!("{:?}", self.get_reg(*reg)?.to_f32());
                print!("{}", str);
                self.stdout += &str;
            }
            Inst::Finish { reg } => {
                self.ret_value = if let Some(reg) = *reg {
                    Some(self.get_reg(reg)?)
                } else {
                    None
                };
                done = true;
            }
        }
        Ok(done)
    }
}

/// Number of dynamic steps allowed in a test program. Useful to detect infinite
/// loops during testing.
const MAX_NUM_STEPS: u32 = 1000000;

#[derive(Debug)]
pub struct RunResult {
    /// Return value.
    pub ret_value: Option<Value>,

    /// Number of dynamically executed steps.
    pub num_steps: usize,

    /// Number of dynamically executed reload instructions.
    pub num_reloads: usize,

    /// Everything that's been printed out.
    pub stdout: String,
}

pub fn run_func(
    f: &Func,
    who: &str,
    reg_universe: &RealRegUniverse,
    run_stage: RunStage,
) -> Result<RunResult, String> {
    println!("");
    println!(
        "Running stage '{}': Func: name='{}' entry='{:?}'",
        who, f.name, f.entry
    );

    if run_stage == RunStage::BeforeRegalloc {
        if let Err(err) = validate(f, reg_universe) {
            return Err(format!("validation error: {}", err));
        }
    }

    let mut istate = IState::new(
        f,
        reg_universe.regs.len(),
        /* max_mem */ 1000,
        run_stage,
    );
    let mut done = false;
    let mut allowed_steps = MAX_NUM_STEPS;
    while allowed_steps > 0 && !done {
        done = istate.step()?;
        allowed_steps -= 1;
    }

    if allowed_steps == 0 {
        return Err("too many dynamic steps. Maybe running an infinite loop?".into());
    }

    println!(
        "Running stage '{}': done.  {} insns, {} spills, {} reloads",
        who, istate.num_insts, istate.num_spills, istate.num_reloads
    );

    Ok(RunResult {
        ret_value: istate.ret_value,
        num_steps: istate.num_insts,
        num_reloads: istate.num_reloads,
        stdout: istate.stdout,
    })
}

//=============================================================================
// Definition of Block and Func, and printing thereof.

#[derive(Debug, Clone)]
pub struct Block {
    pub name: String,
    pub start: InstIx,
    pub len: u32,
    pub estimated_execution_frequency: u16,
}

impl Block {
    pub fn new(name: String, start: InstIx, len: u32) -> Self {
        Self {
            name,
            start,
            len,
            estimated_execution_frequency: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub entry: Option<Label>,
    pub num_virtual_regs: u32,
    pub reftype_reg_start: Option<u32>, // all vregs >= this index are reftyped.
    pub insns: Vec<Inst>,               // indexed by InstIx

    // Note that `blocks` must be in order of increasing `Block::start`
    // fields.  Code that wants to traverse the blocks in some other order
    // must represent the ordering some other way; rearranging Func::blocks is
    // not allowed.
    pub blocks: Vec<Block>, // indexed by BlockIx
}

// Find a block Ix for a block name
fn lookup<'a>(blocks: &Vec<Block>, name: String) -> BlockIx {
    let mut bix = 0;
    for b in blocks.iter() {
        if b.name == name {
            return BlockIx::new(bix);
        }
        bix += 1;
    }
    panic!("Func::lookup: can't resolve label name '{}'", name);
}

impl Func {
    pub fn new(name: &str) -> Self {
        Func {
            name: name.to_string(),
            entry: None,
            num_virtual_regs: 0,
            reftype_reg_start: None,
            insns: vec![],
            blocks: vec![],
        }
    }

    pub fn set_entry(&mut self, entry: &str) {
        self.entry = Some(Label::Unresolved {
            name: entry.to_string(),
        });
    }

    pub fn print(&self, who: &str, mb_block_anns: &Option<Vec<Vec<String>>>) {
        println!("");
        println!(
            "Func {}: name='{}' entry='{:?}' {{",
            who, self.name, self.entry
        );
        let mut ix = 0;
        for b in self.blocks.iter() {
            if ix > 0 {
                println!("");
            }
            println!("  {:?}:{}", BlockIx::new(ix), b.name);

            if let Some(anns_map) = mb_block_anns {
                for ann in &anns_map[ix as usize] {
                    println!("      ;; {}", ann);
                }
            }

            for i in b.start.get()..b.start.get() + b.len {
                let ix = InstIx::new(i);
                println!("      {:<3?}   {:?}", ix, self.insns[ix.get() as usize]);
            }
            ix += 1;
        }
        println!("}}");
    }

    /// Prints the function in a way the parser can comprehend. This is used by
    /// the fuzzer, but not otherwise, so allow dead code here.
    #[allow(dead_code)]
    pub fn render(&self, who: &str, fmt: &mut String) -> fmt::Result {
        use std::fmt::Write;
        use std::iter::FromIterator;

        let mut used_vregs = HashSet::new();
        let mut used_rregs = HashSet::new();

        for b in self.blocks.iter() {
            for i in b.start.get()..b.start.get() + b.len {
                let iix = InstIx::new(i);

                // Get the use, def and mod sets for `self.insns[iix]`.  This is a bit
                // awkward since we don't have the library's internal convenience
                // functions available here.
                let mut ru_vecs = RegUsageCollector::get_empty_reg_vecs_test_framework_only(false);
                let mut ru_collector = RegUsageCollector::new(&mut ru_vecs);

                self.insns[iix.get() as usize].get_reg_usage(&mut ru_collector);
                assert!(!ru_collector.reg_vecs.is_sanitized());

                let (used_vec, defined_vec, modified_vec) =
                    ru_collector.get_use_def_mod_vecs_test_framework_only();
                let used = Set::from_vec(used_vec);
                let defined = Set::from_vec(defined_vec);
                let modified = Set::from_vec(modified_vec);

                for &reg in defined.iter().chain(modified.iter()) {
                    if reg.is_virtual() {
                        used_vregs.insert(reg);
                    } else {
                        used_rregs.insert(reg);
                    }
                }
                for &reg in used.iter() {
                    if reg.is_virtual() {
                        used_vregs.insert(reg);
                    } else {
                        used_rregs.insert(reg);
                    }
                }
            }
        }

        let mut used_vregs = Vec::from_iter(used_vregs.into_iter());
        used_vregs.sort();
        let mut used_rregs = Vec::from_iter(used_rregs.into_iter());
        used_rregs.sort();

        writeln!(fmt, "; {}", who)?;
        if let Some(start) = self.reftype_reg_start {
            writeln!(fmt, "{} = {}", REFTYPE_START, start)?;
        }
        for vreg in used_vregs {
            writeln!(fmt, "{:?} = {:?}", vreg, vreg.get_class())?;
        }
        for rreg in used_rregs {
            writeln!(
                fmt,
                "{:?} = real {:?} {:?}",
                rreg,
                rreg.get_class(),
                rreg.get_index()
            )?;
        }
        writeln!(fmt, "")?;

        let mut ix = 0;
        for b in self.blocks.iter() {
            if ix > 0 {
                writeln!(fmt, "")?;
            }
            writeln!(fmt, "{:?}:", BlockIx::new(ix))?;
            for i in b.start.get()..b.start.get() + b.len {
                let ix = InstIx::new(i);
                writeln!(fmt, "    {:?}", self.insns[ix.get() as usize])?;
            }
            ix += 1;
        }
        writeln!(fmt, "")?;

        Ok(())
    }

    // Get a new VirtualReg name
    pub fn new_virtual_reg(&mut self, rc: RegClass) -> Reg {
        let v = Reg::new_virtual(rc, self.num_virtual_regs);
        self.num_virtual_regs += 1;
        v
    }

    // Add a block to the Func
    pub fn block(&mut self, name: &str, insns: Vec<Inst>) {
        let start = self.insns.len();
        let len = insns.len() as u32;
        self.insns.extend(insns.into_iter());
        let b = Block::new(name.to_string(), InstIx::new(start as u32), len);
        self.blocks.push(b);
    }

    // All blocks have been added.  Resolve labels and we're good to go.
    /* .finish(): check
          - all blocks nonempty
          - all blocks end in i_finish, i_goto or i_goto_ctf
          - no blocks have those insns before the end
          - blocks are in increasing order of ::start fields
          - all referenced blocks actually exist
          - convert references to block numbers
    */
    pub fn finish(&mut self) {
        for bix in BlockIx::new(0).dotdot(BlockIx::new(self.blocks.len() as u32)) {
            let b = &self.blocks[bix.get() as usize];
            if b.len == 0 {
                panic!("Func::done: a block is empty");
            }
            if bix > BlockIx::new(0)
                && self.blocks[bix.minus(1).get() as usize].start
                    >= self.blocks[bix.get() as usize].start
            {
                panic!("Func: blocks are not in increasing order of InstIx");
            }
            for i in 0..b.len {
                let iix = b.start.plus(i);
                if i == b.len - 1 && !self.insns[iix.get() as usize].is_control_flow() {
                    panic!("Func: block must end in control flow insn");
                }
                if i != b.len - 1 && self.insns[iix.get() as usize].is_control_flow() {
                    panic!("Func: block contains control flow insn not at end");
                }
            }
        }

        // Resolve all labels
        let blocks = &self.blocks;
        for i in self.insns.iter_mut() {
            resolve_inst(i, |name| lookup(blocks, name));
        }
        resolve_label(self.entry.as_mut().unwrap(), |name| lookup(blocks, name));
    }

    pub fn update_from_alloc(&mut self, result: regalloc::RegAllocResult<Func>) {
        self.insns = result.insns.into_iter().collect();
        let num_blocks = self.blocks.len();
        let mut i = 0;
        for bix in 0..self.blocks.len() {
            let block = &mut self.blocks[bix];
            block.start = result.target_map[BlockIx::new(bix as u32)];
            block.len = if i + 1 < num_blocks {
                result.target_map[BlockIx::new((i + 1) as u32)].get()
            } else {
                self.insns.len() as u32
            } - block.start.get();
            i += 1;
        }
    }

    pub fn get_stackmap_request(&self) -> Option<StackmapRequestInfo> {
        self.reftype_reg_start.and_then(|reftype_reg_start| {
            if reftype_reg_start == self.num_virtual_regs {
                None
            } else {
                let reftyped_vregs = (reftype_reg_start..self.num_virtual_regs)
                    .map(|index| Reg::new_virtual(RegClass::I32, index).to_virtual_reg())
                    .collect::<Vec<_>>();
                let mut safepoint_insns = vec![];
                for iix in 0..self.insns.len() {
                    match self.insns[iix] {
                        Inst::Safepoint => {
                            safepoint_insns.push(InstIx::new(iix as u32));
                        }
                        _ => {}
                    }
                }
                debug!(
                    "SRI: reftyped_vregs = {:?}, safepoint_insns = {:?}",
                    reftyped_vregs, safepoint_insns
                );
                Some(StackmapRequestInfo {
                    reftype_class: RegClass::I32,
                    reftyped_vregs,
                    safepoint_insns,
                })
            }
        })
    }
}

fn resolve_label<F>(label: &mut Label, lookup: F)
where
    F: Fn(String) -> BlockIx,
{
    let resolved = match label {
        Label::Unresolved { name } => Label::Resolved {
            name: name.clone(),
            bix: lookup(name.clone()),
        },
        Label::Resolved { .. } => panic!("resolveLabel: is already resolved!"),
    };
    *label = resolved;
}

fn resolve_inst<F>(insn: &mut Inst, lookup: F)
where
    F: Copy + Fn(String) -> BlockIx,
{
    match insn {
        Inst::Goto { ref mut target } => resolve_label(target, lookup),
        Inst::GotoCTF {
            cond: _,
            ref mut target_true,
            ref mut target_false,
        } => {
            resolve_label(target_true, lookup);
            resolve_label(target_false, lookup);
        }
        _ => (),
    }
}

#[derive(Clone)]
pub enum Stmt {
    Vanilla {
        insn: Inst,
    },
    IfThenElse {
        cond: Reg,
        stmts_t: Vec<Stmt>,
        stmts_e: Vec<Stmt>,
    },
    RepeatUntil {
        stmts: Vec<Stmt>,
        cond: Reg,
    },
    WhileDo {
        cond: Reg,
        stmts: Vec<Stmt>,
    },
}

// Various handy wrappers, mostly wrappings of i_* functions
pub fn s_if_then_else(cond: Reg, stmts_t: Vec<Stmt>, stmts_e: Vec<Stmt>) -> Stmt {
    Stmt::IfThenElse {
        cond,
        stmts_t,
        stmts_e,
    }
}
pub fn s_if_then(cond: Reg, stmts_t: Vec<Stmt>) -> Stmt {
    Stmt::IfThenElse {
        cond,
        stmts_t,
        stmts_e: vec![],
    }
}
pub fn s_repeat_until(stmts: Vec<Stmt>, cond: Reg) -> Stmt {
    Stmt::RepeatUntil { stmts, cond }
}
pub fn s_while_do(cond: Reg, stmts: Vec<Stmt>) -> Stmt {
    Stmt::WhileDo { cond, stmts }
}

fn s_vanilla(insn: Inst) -> Stmt {
    Stmt::Vanilla { insn }
}

pub fn s_imm(dst: Reg, imm: u32) -> Stmt {
    s_vanilla(i_imm(dst, imm))
}
pub fn s_immf(dst: Reg, imm: f32) -> Stmt {
    s_vanilla(i_immf(dst, imm))
}
pub fn s_copy(dst: Reg, src: Reg) -> Stmt {
    s_vanilla(i_copy(dst, src))
}
pub fn s_load(dst: Reg, addr: AM) -> Stmt {
    s_vanilla(i_load(dst, addr))
}
pub fn s_loadf(dst: Reg, addr: AM) -> Stmt {
    s_vanilla(i_loadf(dst, addr))
}
pub fn s_store(addr: AM, src: Reg) -> Stmt {
    s_vanilla(i_store(addr, src))
}
pub fn s_storef(addr: AM, src: Reg) -> Stmt {
    s_vanilla(i_storef(addr, src))
}
pub fn s_print_s<'a>(str: &'a str) -> Stmt {
    s_vanilla(i_print_s(str))
}
pub fn s_print_i(reg: Reg) -> Stmt {
    s_vanilla(i_print_i(reg))
}
pub fn s_print_f(reg: Reg) -> Stmt {
    s_vanilla(i_print_f(reg))
}

pub fn s_add(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_add(dst, src_left, src_right))
}
pub fn s_sub(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_sub(dst, src_left, src_right))
}
pub fn s_mul(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_mul(dst, src_left, src_right))
}
pub fn s_mod(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_mod(dst, src_left, src_right))
}
pub fn s_shr(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_shr(dst, src_left, src_right))
}
pub fn s_and(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_and(dst, src_left, src_right))
}
pub fn s_cmp_eq(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_cmp_eq(dst, src_left, src_right))
}
pub fn s_cmp_lt(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_cmp_lt(dst, src_left, src_right))
}
pub fn s_cmp_le(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_cmp_le(dst, src_left, src_right))
}
pub fn s_cmp_ge(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_cmp_ge(dst, src_left, src_right))
}
pub fn s_cmp_gt(dst: Reg, src_left: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_cmp_gt(dst, src_left, src_right))
}

pub fn s_addm(dst: Reg, src_right: RI) -> Stmt {
    s_vanilla(i_addm(dst, src_right))
}
//fn s_subm(dst: Reg, src_right: RI) -> Stmt {
//  s_vanilla(i_subm(dst, src_right))
//}

pub fn s_fadd(dst: Reg, src_left: Reg, src_right: Reg) -> Stmt {
    s_vanilla(i_fadd(dst, src_left, src_right))
}
pub fn s_fsub(dst: Reg, src_left: Reg, src_right: Reg) -> Stmt {
    s_vanilla(i_fsub(dst, src_left, src_right))
}
pub fn s_fmul(dst: Reg, src_left: Reg, src_right: Reg) -> Stmt {
    s_vanilla(i_fmul(dst, src_left, src_right))
}
pub fn s_fdiv(dst: Reg, src_left: Reg, src_right: Reg) -> Stmt {
    s_vanilla(i_fdiv(dst, src_left, src_right))
}

//=============================================================================
// The "blockifier".  This is just to make it easier to write test cases, by
// allowing direct use of if-then-else, do-while and repeat-until.  It is
// otherwise entirely unrelated to the register allocator proper.

pub struct Blockifier {
    name: String,
    blocks: Vec<Vec<Inst>>,
    num_virtual_regs: u32,
}

fn make_text_label_str(n: usize) -> String {
    "L".to_string() + &n.to_string()
}

impl Blockifier {
    pub fn new<'b>(name: &str) -> Self {
        Self {
            name: name.to_string(),
            blocks: vec![],
            num_virtual_regs: 0,
        }
    }

    // Get a new VirtualReg name
    pub fn new_virtual_reg(&mut self, rc: RegClass) -> Reg {
        let v = Reg::new_virtual(rc, self.num_virtual_regs);
        self.num_virtual_regs += 1;
        v
    }

    /// Recursive worker function, which flattens out the control flow, producing
    /// a set of blocks.
    fn blockify(&mut self, stmts: Vec<Stmt>) -> (usize, usize) {
        let entry_block = self.blocks.len();
        let mut cur_block = entry_block;
        self.blocks.push(vec![]);
        for s in stmts {
            match s {
                Stmt::Vanilla { insn } => {
                    self.blocks[cur_block].push(insn);
                }
                Stmt::IfThenElse {
                    cond,
                    stmts_t,
                    stmts_e,
                } => {
                    let (t_ent, t_exit) = self.blockify(stmts_t);
                    let (e_ent, e_exit) = self.blockify(stmts_e);
                    let cont = self.blocks.len();
                    self.blocks.push(vec![]);
                    self.blocks[t_exit].push(i_goto(&make_text_label_str(cont)));
                    self.blocks[e_exit].push(i_goto(&make_text_label_str(cont)));
                    self.blocks[cur_block].push(i_goto_ctf(
                        cond,
                        &make_text_label_str(t_ent),
                        &make_text_label_str(e_ent),
                    ));
                    cur_block = cont;
                }
                Stmt::RepeatUntil { stmts, cond } => {
                    let (s_ent, s_exit) = self.blockify(stmts);

                    // Don't create critical edges by creating the following loop
                    // structure:
                    //
                    // current -> loop_header -> s_ent -> s_exit -> after_loop
                    //            ^                       |
                    //            |-------- loop_continue <

                    let loop_header = self.blocks.len();
                    self.blocks.push(vec![i_goto(&make_text_label_str(s_ent))]);

                    self.blocks[cur_block].push(i_goto(&make_text_label_str(loop_header)));

                    let loop_continue = self.blocks.len();
                    self.blocks
                        .push(vec![i_goto(&make_text_label_str(loop_header))]);

                    let after_loop = self.blocks.len();
                    self.blocks.push(vec![]);

                    self.blocks[s_exit].push(i_goto_ctf(
                        cond,
                        &make_text_label_str(after_loop),
                        &make_text_label_str(loop_continue),
                    ));

                    cur_block = after_loop;
                }
                Stmt::WhileDo { cond, stmts } => {
                    let condblock = self.blocks.len();
                    self.blocks.push(vec![]);
                    self.blocks[cur_block].push(i_goto(&make_text_label_str(condblock)));
                    let (s_ent, s_exit) = self.blockify(stmts);
                    self.blocks[s_exit].push(i_goto(&make_text_label_str(condblock)));
                    let cont = self.blocks.len();
                    self.blocks.push(vec![]);
                    self.blocks[condblock].push(i_goto_ctf(
                        cond,
                        &make_text_label_str(s_ent),
                        &make_text_label_str(cont),
                    ));
                    cur_block = cont;
                }
            }
        }
        (entry_block, cur_block)
    }

    // The main external function.  Convert the given statements, into a Func.
    pub fn finish(mut self, stmts: Vec<Stmt>, ret_value: Option<Reg>) -> Func {
        let (ent_bno, exit_bno) = self.blockify(stmts);
        self.blocks[exit_bno].push(i_finish(ret_value));

        // Convert (ent_bno, exit_bno, cleanedUp) into a Func
        let mut func = Func::new(&self.name);
        func.set_entry(&make_text_label_str(ent_bno));
        func.num_virtual_regs = self.num_virtual_regs;
        let mut n = 0;
        for ivec in self.blocks {
            func.block(&make_text_label_str(n), ivec);
            n += 1;
        }

        func.finish();
        func
    }
}

// --------------------------------------------------
// Implementation of `Function` trait for test cases.

impl regalloc::Function for Func {
    type Inst = Inst;

    fn insns(&self) -> &[Inst] {
        &self.insns[..]
    }

    fn insns_mut(&mut self) -> &mut [Inst] {
        &mut self.insns[..]
    }

    fn get_insn(&self, iix: InstIx) -> &Inst {
        &self.insns[iix.get() as usize]
    }

    fn get_insn_mut(&mut self, iix: InstIx) -> &mut Inst {
        &mut self.insns[iix.get() as usize]
    }

    fn entry_block(&self) -> BlockIx {
        BlockIx::new(0)
    }

    fn blocks(&self) -> Range<BlockIx> {
        Range::new(BlockIx::new(0), self.blocks.len())
    }

    /// Provide the range of instruction indices contained in each block.
    fn block_insns(&self, block: BlockIx) -> Range<InstIx> {
        Range::new(
            self.blocks[block.get() as usize].start,
            self.blocks[block.get() as usize].len as usize,
        )
    }

    /// Get CFG successors: indexed by block, provide a list of successor blocks.
    fn block_succs(&self, block: BlockIx) -> Cow<[BlockIx]> {
        let last_insn = self.blocks[block.get() as usize]
            .start
            .plus(self.blocks[block.get() as usize].len - 1);
        Cow::Owned(self.insns[last_insn.get() as usize].get_targets())
    }

    fn is_ret(&self, insn: InstIx) -> bool {
        match &self.insns[insn.get() as usize] {
            &Inst::Finish { .. } => true,
            _ => false,
        }
    }

    /// Provide the defined, used, and modified registers for an instruction.
    fn get_regs(insn: &Self::Inst, collector: &mut RegUsageCollector) {
        insn.get_reg_usage(collector);
    }

    /// Map each register slot through a virt -> phys mapping indexed
    /// by virtual register. The two separate maps provide the
    /// mapping to use for uses (which semantically occur just prior
    /// to the instruction's effect) and defs (which semantically occur
    /// just after the instruction's effect). Regs that were "modified"
    /// can use either map; the vreg should be the same in both.
    fn map_regs<RUM: RegUsageMapper>(insn: &mut Self::Inst, maps: &RUM) {
        insn.map_regs(maps);
    }

    /// Allow the regalloc to query whether this is a move.
    fn is_move(&self, insn: &Self::Inst) -> Option<(Writable<Reg>, Reg)> {
        match insn {
            &Inst::Copy { dst, src } => Some((Writable::from_reg(dst), src)),
            _ => None,
        }
    }

    fn get_num_vregs(&self) -> usize {
        self.num_virtual_regs as usize
    }

    /// How many logical spill slots does the given regclass require?  E.g., on a
    /// 64-bit machine, spill slots may nominally be 64-bit words, but a 128-bit
    /// vector value will require two slots.  The regalloc will always align on
    /// this size.
    fn get_spillslot_size(&self, _regclass: RegClass, _for_vreg: VirtualReg) -> u32 {
        // For our simple test ISA, every value occupies one spill slot.
        1
    }

    /// Generate a spill instruction for insertion into the instruction sequence.
    fn gen_spill(
        &self,
        to_slot: SpillSlot,
        from_reg: RealReg,
        _for_vreg: Option<VirtualReg>,
    ) -> Self::Inst {
        match from_reg.get_class() {
            RegClass::I32 => i_spill(to_slot, from_reg),
            RegClass::F32 => i_spillf(to_slot, from_reg),
            _ => panic!("Unused register class in test ISA was used"),
        }
    }

    /// Generate a reload instruction for insertion into the instruction sequence.
    fn gen_reload(
        &self,
        to_reg: Writable<RealReg>,
        from_slot: SpillSlot,
        _for_vreg: Option<VirtualReg>,
    ) -> Self::Inst {
        match to_reg.to_reg().get_class() {
            RegClass::I32 => i_reload(to_reg.to_reg(), from_slot),
            RegClass::F32 => i_reloadf(to_reg.to_reg(), from_slot),
            _ => panic!("Unused register class in test ISA was used"),
        }
    }

    /// Generate a register-to-register move for insertion into the instruction
    /// sequence.
    fn gen_move(
        &self,
        to_reg: Writable<RealReg>,
        from_reg: RealReg,
        _for_vreg: VirtualReg,
    ) -> Self::Inst {
        match to_reg.to_reg().get_class() {
            RegClass::I32 => Inst::Copy {
                src: from_reg.to_reg(),
                dst: to_reg.to_reg().to_reg(),
            },
            RegClass::F32 => Inst::CopyF {
                src: from_reg.to_reg(),
                dst: to_reg.to_reg().to_reg(),
            },
            _ => unimplemented!("gen_move for non i32/f32"),
        }
    }

    /// Generate an instruction which is a no-op and has zero length.
    fn gen_zero_len_nop(&self) -> Self::Inst {
        Inst::NopZ {}
    }

    /// Try to alter an existing instruction to use a value directly in a
    /// spillslot (accessing memory directly) instead of the given register. May
    /// be useful on ISAs that have mem/reg ops, like x86.
    ///
    /// Note that this is not *quite* just fusing a load with the op; if the
    /// value is def'd or modified, it should be written back to the spill slot
    /// as well. In other words, it is just using the spillslot as if it were a
    /// real register, for reads and/or writes.
    fn maybe_direct_reload(
        &self,
        _insn: &Self::Inst,
        _reg: VirtualReg,
        _slot: SpillSlot,
    ) -> Option<Self::Inst> {
        // test ISA does not have register-memory ALU instruction forms.
        None
    }

    fn func_liveins(&self) -> Set<RealReg> {
        Set::empty()
    }

    fn func_liveouts(&self) -> Set<RealReg> {
        Set::empty()
    }
}

/// Create a universe for testing, with nI32 `I32` class regs and nF32 `F32`
/// class regs.
pub fn make_universe(num_i32: usize, num_f32: usize) -> RealRegUniverse {
    let total_regs = num_i32 + num_f32;
    if total_regs >= 256 {
        panic!("make_universe: too many regs, cannot represent");
    }

    let mut regs = Vec::<(RealReg, String)>::new();
    let mut allocable_by_class = [None; NUM_REG_CLASSES];
    let mut index = 0u8;

    if num_i32 > 0 {
        let first = index as usize;
        for i in 0..num_i32 {
            let name = format!("R{}", i).to_string();
            let reg = Reg::new_real(RegClass::I32, /*enc=*/ 0, index).to_real_reg();
            regs.push((reg, name));
            index += 1;
        }
        let last = index as usize - 1;
        allocable_by_class[RegClass::I32.rc_to_usize()] = Some(RegClassInfo {
            first,
            last,
            suggested_scratch: Some(last),
        });
    }

    if num_f32 > 0 {
        let first = index as usize;
        for i in 0..num_f32 {
            let name = format!("F{}", i).to_string();
            let reg = Reg::new_real(RegClass::F32, /*enc=*/ 0, index).to_real_reg();
            regs.push((reg, name));
            index += 1;
        }
        let last = index as usize - 1;
        allocable_by_class[RegClass::F32.rc_to_usize()] = Some(RegClassInfo {
            first,
            last,
            suggested_scratch: Some(last),
        });
    }

    debug_assert!(index as usize == total_regs);

    let allocable = regs.len();
    let univ = RealRegUniverse {
        regs,
        // for this example, all regs are allocable
        allocable,
        allocable_by_class,
    };
    univ.check_is_sane();

    univ
}
