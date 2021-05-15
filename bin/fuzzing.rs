//! Implements fuzzing primitives for everything.

use arbitrary::{Arbitrary, Result, Unstructured};
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;

use crate::test_framework::{self as ir, *};
use regalloc::*;

pub const NUM_REAL_REGS_PER_RC: u8 = 4;
const NUM_REG_CLASSES: u32 = 5;

/// Maximum number of vregs.
const NUM_VREGS: u16 = 16;
/// Maximum number of blocks.
const NUM_BLOCKS: u8 = 8;
/// Maximum number of instructions per block.
const NUM_BLOCK_INSTS: u8 = 8;

struct FuzzingEnv {
    num_blocks: u8,
    num_virtual_regs: u16,
    num_reftyped_regs: u16, // numbered in vreg space above ordinary vregs.
    /// Map of virtual register index to register class. None means the register hasn't been ever defined.
    vregs: HashMap<u16, RegClass>,
    /// Set of reftyped vregs that have been defined.
    reftyped_regs: HashSet<u16>,
    /// Really a hashmap from rc to HashSet<Reg>.
    regs_by_rc: Vec<HashSet<Reg>>,
    vregs_by_rc: Vec<HashSet<u16>>,
}

impl FuzzingEnv {
    fn block(&self, u: &mut Unstructured) -> Result<BlockIx> {
        Ok(BlockIx::new((u8::arbitrary(u)? % self.num_blocks) as u32))
    }

    fn label(&self, u: &mut Unstructured) -> Result<Label> {
        let bix = self.block(u)?;
        Ok(Label::Resolved {
            name: format!("{:?}", bix),
            bix,
        })
    }

    /// Returns true whenever a register of the given register class may be used.
    fn can_use_reg(&self, rc: RegClass) -> bool {
        !self.regs_by_rc[rc as usize].is_empty()
    }

    /// Returns true whenever a reftyped register may be used.
    fn can_use_reftyped_reg(&self) -> bool {
        !self.reftyped_regs.is_empty()
    }

    /// Returns true whenever a register of the given register class may be defined.
    fn can_def_reg(&self, rc: RegClass) -> bool {
        // If we can use one with the given reg class, then we can redefine it!
        self.can_use_reg(rc) || self.vregs.len() != (self.num_virtual_regs as usize)
    }

    /// Returns true whenever a virtual register of the given register class may be defined.
    fn can_def_vreg(&self, rc: RegClass) -> bool {
        !self.vregs_by_rc[rc as usize].is_empty()
            || self.vregs.len() != (self.num_virtual_regs as usize)
    }

    /// Returns true whenever a reftyped vreg may be defined.
    fn can_def_reftyped_reg(&self) -> bool {
        self.can_use_reftyped_reg() || self.reftyped_regs.len() != (self.num_reftyped_regs as usize)
    }

    fn def_reg(&mut self, rc: RegClass, u: &mut Unstructured) -> Result<Reg> {
        debug_assert!(self.can_def_reg(rc));
        let reg = if self.can_def_vreg(rc) && bool::arbitrary(u)? {
            // virtual.
            let mut index = u16::arbitrary(u)? % self.num_virtual_regs;
            while self.vregs.contains_key(&index) && self.vregs[&index] != rc {
                // linear probing!
                index = (index + 1) % self.num_virtual_regs;
            }
            self.vregs.insert(index, rc);
            self.vregs_by_rc[rc as usize].insert(index);
            Reg::new_virtual(rc, index as u32)
        } else {
            // real.
            // TODO there's insider knowledge about the real reg universe stuck here.
            let index = match rc {
                RegClass::I32 => 0,
                RegClass::F32 => NUM_REAL_REGS_PER_RC,
                _ => panic!("unexpected rc"),
            } + u8::arbitrary(u)? % NUM_REAL_REGS_PER_RC;
            Reg::new_real(rc, 0x0, index)
        };
        self.regs_by_rc[rc as usize].insert(reg);
        Ok(reg)
    }

    fn def_reftyped_reg(&mut self, u: &mut Unstructured) -> Result<Reg> {
        debug_assert!(self.can_def_reftyped_reg());
        if self.reftyped_regs.len() == 0
            || (self.reftyped_regs.len() < (self.num_reftyped_regs as usize) && bool::arbitrary(u)?)
        {
            let mut index = u16::arbitrary(u)? % self.num_reftyped_regs;
            while self.reftyped_regs.contains(&index) {
                index = (index + 1) % self.num_reftyped_regs;
            }
            self.reftyped_regs.insert(index);
            let index = index + self.num_virtual_regs;
            Ok(Reg::new_virtual(RegClass::I32, index as u32))
        } else {
            assert!(self.reftyped_regs.len() > 0);
            let list_index = usize::arbitrary(u)? % self.reftyped_regs.len();
            let reg_index = self
                .reftyped_regs
                .iter()
                .skip(list_index)
                .cloned()
                .next()
                .unwrap();
            let reg_index = reg_index + self.num_virtual_regs;
            Ok(Reg::new_virtual(RegClass::I32, reg_index as u32))
        }
    }

    fn mod_reg(&mut self, rc: RegClass, u: &mut Unstructured) -> Result<Reg> {
        // No need to handle the def part! If there was such a register, it was inserted in the first
        // place with the same register class.
        self.get_reg(rc, u)
    }

    fn get_reg(&self, rc: RegClass, u: &mut Unstructured) -> Result<Reg> {
        debug_assert!(self.can_use_reg(rc));
        let regs = Vec::from_iter(self.regs_by_rc[rc as usize].iter());
        let reg = *regs[usize::arbitrary(u)? % regs.len()];
        Ok(reg)
    }

    fn get_reftyped_reg(&self, u: &mut Unstructured) -> Result<Reg> {
        debug_assert!(self.can_use_reftyped_reg());
        let regs = Vec::from_iter(self.reftyped_regs.iter());
        let reg_index = *regs[usize::arbitrary(u)? % regs.len()];
        let reg_index = reg_index + self.num_virtual_regs;
        Ok(Reg::new_virtual(RegClass::I32, reg_index as u32))
    }

    fn get_ri(&self, u: &mut Unstructured) -> Result<RI> {
        Ok(if self.can_use_reg(RegClass::I32) && bool::arbitrary(u)? {
            RI::Reg {
                reg: self.get_reg(RegClass::I32, u)?,
            }
        } else {
            RI::Imm {
                imm: u32::arbitrary(u)?,
            }
        })
    }

    fn get_am(&self, u: &mut Unstructured) -> Result<AM> {
        debug_assert!(self.can_use_reg(RegClass::I32));
        let base = self.get_reg(RegClass::I32, u)?;
        Ok(if bool::arbitrary(u)? {
            // RI
            AM::RI {
                base,
                offset: u32::arbitrary(u)?,
            }
        } else {
            // RR
            let offset = self.get_reg(RegClass::I32, u)?;
            AM::RR { base, offset }
        })
    }

    fn inst(&mut self, u: &mut Unstructured) -> Result<Inst> {
        use Inst::*;
        use RegClass::*;

        enum AllowedInst {
            Imm,
            ImmF,
            Copy,
            CopyF,
            CopyRef,
            BinOp,
            BinOpM,
            BinOpF,
            Load,
            LoadF,
            Store,
            StoreF,
            MakeRef,
            UseRef,
        }

        let mut allowed_insts = Vec::new();
        if self.can_def_reg(I32) {
            allowed_insts.push(AllowedInst::Imm);
        }
        if self.can_def_reg(F32) {
            allowed_insts.push(AllowedInst::ImmF);
        }
        if self.can_use_reg(I32) {
            allowed_insts.push(AllowedInst::Copy);
            allowed_insts.push(AllowedInst::BinOp);
            allowed_insts.push(AllowedInst::BinOpM);
            allowed_insts.push(AllowedInst::Load);
            allowed_insts.push(AllowedInst::Store);
            if self.can_def_reg(F32) {
                allowed_insts.push(AllowedInst::LoadF);
            }
            if self.can_use_reg(F32) {
                allowed_insts.push(AllowedInst::StoreF);
            }
        }
        if self.can_use_reg(F32) {
            allowed_insts.push(AllowedInst::CopyF);
            allowed_insts.push(AllowedInst::BinOpF);
        }
        if self.can_def_reftyped_reg() && self.can_use_reg(I32) {
            allowed_insts.push(AllowedInst::MakeRef);
        }
        if self.can_use_reftyped_reg() && self.can_def_reg(I32) {
            allowed_insts.push(AllowedInst::UseRef);
        }
        if self.can_def_reftyped_reg() && self.can_use_reftyped_reg() {
            allowed_insts.push(AllowedInst::CopyRef);
        }

        debug_assert!(!allowed_insts.is_empty());

        let index = u8::arbitrary(u)? as usize % (allowed_insts.len() + 1);
        if index == allowed_insts.len() {
            return self.inst_control_flow(u);
        }

        // Get uses before defs!
        Ok(match allowed_insts[index] {
            AllowedInst::Imm => Imm {
                dst: self.def_reg(I32, u)?,
                imm: u32::arbitrary(u)?,
            },
            AllowedInst::ImmF => ImmF {
                dst: self.def_reg(F32, u)?,
                imm: f32::arbitrary(u)?,
            },
            AllowedInst::Copy => {
                let src = self.get_reg(I32, u)?;
                Copy {
                    dst: self.def_reg(I32, u)?,
                    src,
                }
            }
            AllowedInst::CopyF => {
                let src = self.get_reg(F32, u)?;
                CopyF {
                    dst: self.def_reg(F32, u)?,
                    src,
                }
            }
            AllowedInst::CopyRef => {
                let dst = self.def_reftyped_reg(u)?;
                let src = self.get_reftyped_reg(u)?;
                Copy { dst, src }
            }
            AllowedInst::BinOp => {
                let src_left = self.get_reg(I32, u)?;
                let src_right = self.get_ri(u)?;
                BinOp {
                    op: ir::BinOp::arbitrary(u)?,
                    dst: self.def_reg(I32, u)?,
                    src_left,
                    src_right,
                }
            }
            AllowedInst::BinOpM => BinOpM {
                op: ir::BinOp::arbitrary(u)?,
                dst: self.mod_reg(I32, u)?,
                src_right: self.get_ri(u)?,
            },
            AllowedInst::BinOpF => {
                let src_left = self.get_reg(F32, u)?;
                let src_right = self.get_reg(F32, u)?;
                BinOpF {
                    op: ir::BinOpF::arbitrary(u)?,
                    dst: self.def_reg(F32, u)?,
                    src_left,
                    src_right,
                }
            }
            AllowedInst::Load => {
                let addr = self.get_am(u)?;
                Load {
                    dst: self.def_reg(I32, u)?,
                    addr,
                }
            }
            AllowedInst::LoadF => {
                let addr = self.get_am(u)?;
                LoadF {
                    dst: self.def_reg(F32, u)?,
                    addr,
                }
            }
            AllowedInst::Store => Store {
                addr: self.get_am(u)?,
                src: self.get_reg(I32, u)?,
            },
            AllowedInst::StoreF => StoreF {
                addr: self.get_am(u)?,
                src: self.get_reg(F32, u)?,
            },
            AllowedInst::MakeRef => MakeRef {
                dst: self.def_reftyped_reg(u)?,
                src: self.get_reg(I32, u)?,
            },
            AllowedInst::UseRef => UseRef {
                dst: self.def_reg(I32, u)?,
                src: self.get_reftyped_reg(u)?,
            },
        })
    }

    fn inst_control_flow(&self, u: &mut Unstructured) -> Result<Inst> {
        use Inst::*;
        use RegClass::*;

        enum AllowedInst {
            Goto,
            GotoCtf,
            Finish,
        }

        let mut allowed_insts = vec![AllowedInst::Goto, AllowedInst::Finish];
        if self.can_use_reg(I32) {
            allowed_insts.push(AllowedInst::GotoCtf);
        }

        Ok(
            match allowed_insts[u8::arbitrary(u)? as usize % allowed_insts.len()] {
                AllowedInst::GotoCtf => GotoCTF {
                    cond: self.get_reg(I32, u)?,
                    target_true: self.label(u)?,
                    target_false: self.label(u)?,
                },
                AllowedInst::Goto => Goto {
                    target: self.label(u)?,
                },
                AllowedInst::Finish => {
                    let ret_value = if bool::arbitrary(u)? {
                        if self.can_use_reg(I32) {
                            Some(self.get_reg(I32, u)?)
                        } else if self.can_use_reg(F32) {
                            Some(self.get_reg(F32, u)?)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    Finish { reg: ret_value }
                }
            },
        )
    }
}

impl Arbitrary for Func {
    fn arbitrary(u: &mut Unstructured) -> arbitrary::Result<Func> {
        let num_virtual_regs = 1 + (u16::arbitrary(u)? % NUM_VREGS);
        let num_reftyped_regs = 1 + (u16::arbitrary(u)? % NUM_VREGS);
        let mut num_blocks = 1 + (u8::arbitrary(u)? % NUM_BLOCKS);

        let mut env = FuzzingEnv {
            num_blocks,
            num_virtual_regs,
            num_reftyped_regs,
            vregs: HashMap::new(),
            reftyped_regs: HashSet::new(),
            regs_by_rc: vec![HashSet::new(); NUM_REG_CLASSES as usize],
            vregs_by_rc: vec![HashSet::new(); NUM_REG_CLASSES as usize],
        };

        let entry = Some(Label::Resolved {
            name: "entry".to_string(),
            bix: BlockIx::new(0),
        });

        let mut insts = vec![];
        let mut blocks = vec![];

        let mut cur_block = 0;

        while num_blocks > 0 {
            let start = insts.len();

            let mut num_block_insts = 1 + (u8::arbitrary(u)? % NUM_BLOCK_INSTS);

            if bool::arbitrary(u)? {
                insts.push(Inst::Safepoint);
            }
            while num_block_insts > 0 {
                let inst = if num_block_insts == 1 {
                    env.inst_control_flow(u)?
                } else {
                    env.inst(u)?
                };
                let is_control_flow = inst.is_control_flow();
                insts.push(inst);
                num_block_insts -= 1;
                if is_control_flow {
                    break;
                }
            }

            debug_assert!(insts.len() > start);
            let len = insts.len() - start;
            let block = Block {
                name: format!("b{}", cur_block),
                start: InstIx::new(start as u32),
                len: len as u32,
                estimated_execution_frequency: 0,
            };
            blocks.push(block);

            cur_block += 1;
            num_blocks -= 1;
        }

        Ok(Func {
            name: "funk".to_string(),
            entry,
            num_virtual_regs: (num_virtual_regs + num_reftyped_regs) as u32,
            reftype_reg_start: Some(num_virtual_regs as u32),
            insns: insts,
            blocks,
        })
    }
}
