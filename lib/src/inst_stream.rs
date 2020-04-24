#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

use crate::checker::Inst as CheckerInst;
use crate::checker::{CheckerContext, CheckerErrors};
use crate::data_structures::{
    BlockIx, InstIx, InstPoint, Map, RangeFrag, RangeFragIx, RealReg, RealRegUniverse, SpillSlot,
    TypedIxVec, VirtualReg, Writable,
};
use crate::{Function, RegAllocError};
use log::trace;

use std::result::Result;

//=============================================================================
// InstToInsert and InstToInsertAndPoint

#[derive(Clone, Debug)]
pub(crate) enum InstToInsert {
    Spill {
        to_slot: SpillSlot,
        from_reg: RealReg,
        for_vreg: VirtualReg,
    },
    Reload {
        to_reg: Writable<RealReg>,
        from_slot: SpillSlot,
        for_vreg: VirtualReg,
    },
    Move {
        to_reg: Writable<RealReg>,
        from_reg: RealReg,
        for_vreg: VirtualReg,
    },
}

impl InstToInsert {
    pub(crate) fn construct<F: Function>(&self, f: &F) -> F::Inst {
        match self {
            &InstToInsert::Spill {
                to_slot,
                from_reg,
                for_vreg,
            } => f.gen_spill(to_slot, from_reg, for_vreg),
            &InstToInsert::Reload {
                to_reg,
                from_slot,
                for_vreg,
            } => f.gen_reload(to_reg, from_slot, for_vreg),
            &InstToInsert::Move {
                to_reg,
                from_reg,
                for_vreg,
            } => f.gen_move(to_reg, from_reg, for_vreg),
        }
    }

    pub(crate) fn to_checker_inst(&self) -> CheckerInst {
        match self {
            &InstToInsert::Spill {
                to_slot, from_reg, ..
            } => CheckerInst::Spill {
                into: to_slot,
                from: from_reg,
            },
            &InstToInsert::Reload {
                to_reg, from_slot, ..
            } => CheckerInst::Reload {
                into: to_reg,
                from: from_slot,
            },
            &InstToInsert::Move {
                to_reg, from_reg, ..
            } => CheckerInst::Move {
                into: to_reg,
                from: from_reg,
            },
        }
    }
}

pub(crate) struct InstToInsertAndPoint {
    pub(crate) inst: InstToInsert,
    pub(crate) point: InstPoint,
}

impl InstToInsertAndPoint {
    pub(crate) fn new(inst: InstToInsert, point: InstPoint) -> Self {
        Self { inst, point }
    }
}

//=============================================================================
// Apply all vreg->rreg mappings for the function's instructions, and run
// the checker if required.  This also removes instructions that the core
// algorithm wants removed, by nop-ing them out.

#[inline(never)]
fn map_vregs_to_rregs<F: Function>(
    func: &mut F,
    frag_map: Vec<(RangeFragIx, VirtualReg, RealReg)>,
    frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
    insts_to_add: &Vec<InstToInsertAndPoint>,
    iixs_to_nop_out: &Vec<InstIx>,
    reg_universe: &RealRegUniverse,
    has_multiple_blocks_per_frag: bool,
    use_checker: bool,
) -> Result<(), CheckerErrors> {
    // Set up checker state, if indicated by our configuration.
    let mut checker: Option<CheckerContext> = None;
    let mut insn_blocks: Vec<BlockIx> = vec![];
    if use_checker {
        checker = Some(CheckerContext::new(func, reg_universe, insts_to_add));
        insn_blocks.resize(func.insns().len(), BlockIx::new(0));
        for blockIx in func.blocks() {
            for insnIx in func.block_insns(blockIx) {
                insn_blocks[insnIx.get() as usize] = blockIx;
            }
        }
    }

    // Sort the insn nop-out index list, so we can advance through it
    // during the main loop.
    let mut iixs_to_nop_out = iixs_to_nop_out.clone();
    iixs_to_nop_out.sort();

    // Make two copies of the fragment mapping, one sorted by the fragment start
    // points (just the InstIx numbers, ignoring the Point), and one sorted by
    // fragment end points.
    let mut frag_maps_by_start = frag_map.clone();
    let mut frag_maps_by_end = frag_map;

    // -------- Edit the instruction stream --------
    frag_maps_by_start.sort_unstable_by(|(frag_ix, _, _), (other_frag_ix, _, _)| {
        frag_env[*frag_ix]
            .first
            .iix
            .partial_cmp(&frag_env[*other_frag_ix].first.iix)
            .unwrap()
    });

    frag_maps_by_end.sort_unstable_by(|(frag_ix, _, _), (other_frag_ix, _, _)| {
        frag_env[*frag_ix]
            .last
            .iix
            .partial_cmp(&frag_env[*other_frag_ix].last.iix)
            .unwrap()
    });

    //debug!("Firsts: {}", fragMapsByStart.show());
    //debug!("Lasts:  {}", fragMapsByEnd.show());

    let mut cursor_starts = 0;
    let mut cursor_ends = 0;
    let mut cursor_nop = 0;

    let mut map = Map::<VirtualReg, RealReg>::default();

    //fn showMap(m: &Map<VirtualReg, RealReg>) -> String {
    //  let mut vec: Vec<(&VirtualReg, &RealReg)> = m.iter().collect();
    //  vec.sort_by(|p1, p2| p1.0.partial_cmp(p2.0).unwrap());
    //  format!("{:?}", vec)
    //}

    fn is_sane(frag: &RangeFrag) -> bool {
        // "Normal" frag (unrelated to spilling).  No normal frag may start or
        // end at a .s or a .r point.
        if frag.first.pt.is_use_or_def()
            && frag.last.pt.is_use_or_def()
            && frag.first.iix <= frag.last.iix
        {
            return true;
        }
        // A spill-related ("bridge") frag.  There are three possibilities,
        // and they correspond exactly to `BridgeKind`.
        if frag.first.pt.is_reload() && frag.last.pt.is_use() && frag.last.iix == frag.first.iix {
            // BridgeKind::RtoU
            return true;
        }
        if frag.first.pt.is_reload() && frag.last.pt.is_spill() && frag.last.iix == frag.first.iix {
            // BridgeKind::RtoS
            return true;
        }
        if frag.first.pt.is_def() && frag.last.pt.is_spill() && frag.last.iix == frag.first.iix {
            // BridgeKind::DtoS
            return true;
        }
        // None of the above apply.  This RangeFrag is insane \o/
        false
    }

    let mut lastInsnIx = -1;
    for insnIx in func.insn_indices() {
        // Ensure instruction indices are in order. Logic below requires this.
        assert!(insnIx.get() as i32 > lastInsnIx);
        lastInsnIx = insnIx.get() as i32;

        //debug!("");
        //debug!("QQQQ insn {}: {}",
        //         insnIx, func.insns[insnIx].show());
        //debug!("QQQQ init map {}", showMap(&map));
        // advance [cursorStarts, +numStarts) to the group for insnIx
        while cursor_starts < frag_maps_by_start.len()
            && frag_env[frag_maps_by_start[cursor_starts].0].first.iix < insnIx
        {
            cursor_starts += 1;
        }
        let mut numStarts = 0;
        while cursor_starts + numStarts < frag_maps_by_start.len()
            && frag_env[frag_maps_by_start[cursor_starts + numStarts].0]
                .first
                .iix
                == insnIx
        {
            numStarts += 1;
        }

        // advance [cursorEnds, +numEnds) to the group for insnIx
        while cursor_ends < frag_maps_by_end.len()
            && frag_env[frag_maps_by_end[cursor_ends].0].last.iix < insnIx
        {
            cursor_ends += 1;
        }
        let mut numEnds = 0;
        while cursor_ends + numEnds < frag_maps_by_end.len()
            && frag_env[frag_maps_by_end[cursor_ends + numEnds].0].last.iix == insnIx
        {
            numEnds += 1;
        }

        // advance cursor_nop in the iixs_to_nop_out list.
        while cursor_nop < iixs_to_nop_out.len() && iixs_to_nop_out[cursor_nop] < insnIx {
            cursor_nop += 1;
        }

        let nop_this_insn =
            cursor_nop < iixs_to_nop_out.len() && iixs_to_nop_out[cursor_nop] == insnIx;

        // So now, fragMapsByStart[cursorStarts, +numStarts) are the mappings
        // for fragments that begin at this instruction, in no particular
        // order.  And fragMapsByEnd[cursorEnd, +numEnd) are the RangeFragIxs
        // for fragments that end at this instruction.

        //debug!("insn no {}:", insnIx);
        //for j in cursorStarts .. cursorStarts + numStarts {
        //    debug!("   s: {} {}",
        //             (fragMapsByStart[j].1, fragMapsByStart[j].2).show(),
        //             frag_env[ fragMapsByStart[j].0 ]
        //             .show());
        //}
        //for j in cursorEnds .. cursorEnds + numEnds {
        //    debug!("   e: {} {}",
        //             (fragMapsByEnd[j].1, fragMapsByEnd[j].2).show(),
        //             frag_env[ fragMapsByEnd[j].0 ]
        //             .show());
        //}

        // Sanity check all frags.  In particular, reload and spill frags are
        // heavily constrained.  No functional effect.
        for j in cursor_starts..cursor_starts + numStarts {
            let frag = &frag_env[frag_maps_by_start[j].0];
            // "It really starts here, as claimed."
            debug_assert!(frag.first.iix == insnIx);
            debug_assert!(is_sane(&frag));
        }
        for j in cursor_ends..cursor_ends + numEnds {
            let frag = &frag_env[frag_maps_by_end[j].0];
            // "It really ends here, as claimed."
            debug_assert!(frag.last.iix == insnIx);
            debug_assert!(is_sane(frag));
        }

        // Here's the plan, in summary:
        // Update map for I.r:
        //   add frags starting at I.r
        //   no frags should end at I.r (it's a reload insn)
        // Update map for I.u:
        //   add frags starting at I.u
        //   map_uses := map
        //   remove frags ending at I.u
        // Update map for I.d:
        //   add frags starting at I.d
        //   map_defs := map
        //   remove frags ending at I.d
        // Update map for I.s:
        //   no frags should start at I.s (it's a spill insn)
        //   remove frags ending at I.s
        // apply map_uses/map_defs to I

        //debug!("QQQQ mapping insn {:?}", insnIx);
        //debug!("QQQQ current map {}", showMap(&map));
        trace!("current map {:?}", map);

        // Update map for I.r:
        //   add frags starting at I.r
        //   no frags should end at I.r (it's a reload insn)
        for j in cursor_starts..cursor_starts + numStarts {
            let frag = &frag_env[frag_maps_by_start[j].0];
            if frag.first.pt.is_reload() {
                //////// STARTS at I.r
                map.insert(frag_maps_by_start[j].1, frag_maps_by_start[j].2);
                //debug!(
                //  "QQQQ inserted frag from reload: {:?} -> {:?}",
                //  fragMapsByStart[j].1, fragMapsByStart[j].2
                //);
            }
        }

        // Update map for I.u:
        //   add frags starting at I.u
        //   map_uses := map
        //   remove frags ending at I.u
        for j in cursor_starts..cursor_starts + numStarts {
            let frag = &frag_env[frag_maps_by_start[j].0];
            if frag.first.pt.is_use() {
                //////// STARTS at I.u
                map.insert(frag_maps_by_start[j].1, frag_maps_by_start[j].2);
                //debug!(
                //  "QQQQ inserted frag from use: {:?} -> {:?}",
                //  fragMapsByStart[j].1, fragMapsByStart[j].2
                //);
            }
        }
        let map_uses = map.clone();
        for j in cursor_ends..cursor_ends + numEnds {
            let frag = &frag_env[frag_maps_by_end[j].0];
            if frag.last.pt.is_use() {
                //////// ENDS at I.U
                map.remove(&frag_maps_by_end[j].1);
                //debug!("QQQQ removed frag after use: {:?}", fragMapsByStart[j].1);
            }
        }

        trace!("use map {:?}", map_uses);
        trace!("map after I.u {:?}", map);

        // Update map for I.d:
        //   add frags starting at I.d
        //   map_defs := map
        //   remove frags ending at I.d
        for j in cursor_starts..cursor_starts + numStarts {
            let frag = &frag_env[frag_maps_by_start[j].0];
            if frag.first.pt.is_def() {
                //////// STARTS at I.d
                map.insert(frag_maps_by_start[j].1, frag_maps_by_start[j].2);
                //debug!(
                //  "QQQQ inserted frag from def: {:?} -> {:?}",
                //  fragMapsByStart[j].1, fragMapsByStart[j].2
                //);
            }
        }
        let map_defs = map.clone();
        for j in cursor_ends..cursor_ends + numEnds {
            let frag = &frag_env[frag_maps_by_end[j].0];
            if frag.last.pt.is_def() {
                //////// ENDS at I.d
                map.remove(&frag_maps_by_end[j].1);
                //debug!("QQQQ ended frag from def: {:?}", fragMapsByEnd[j].1);
            }
        }

        trace!("map defs {:?}", map_defs);
        trace!("map after I.d {:?}", map);

        // Update map for I.s:
        //   no frags should start at I.s (it's a spill insn)
        //   remove frags ending at I.s
        for j in cursor_ends..cursor_ends + numEnds {
            let frag = &frag_env[frag_maps_by_end[j].0];
            if frag.last.pt.is_spill() {
                //////// ENDS at I.s
                map.remove(&frag_maps_by_end[j].1);
                //debug!("QQQQ ended frag from spill: {:?}", fragMapsByEnd[j].1);
            }
        }

        //debug!("QQQQ map_uses {}", showMap(&map_uses));
        //debug!("QQQQ map_defs {}", showMap(&map_defs));

        // If we have a checker, update it with spills, reloads, moves, and this
        // instruction, while we have `map_uses` and `map_defs` available.
        if let &mut Some(ref mut checker) = &mut checker {
            let blockIx = insn_blocks[insnIx.get() as usize];
            checker.handle_insn(reg_universe, func, blockIx, insnIx, &map_uses, &map_defs)?;

            // We only build the insn->block index when the checker is enabled, so
            // we can only perform this debug-assert here.
            if func.block_insns(blockIx).last() == insnIx {
                //debug!("Block end");
                debug_assert!(has_multiple_blocks_per_frag || map.is_empty());
            }
        }

        // Finally, we have map_uses/map_defs set correctly for this instruction.
        // Apply it.
        if !nop_this_insn {
            trace!("map_regs for {:?}", insnIx);
            let mut insn = func.get_insn_mut(insnIx);
            F::map_regs(&mut insn, &map_uses, &map_defs);
            trace!("mapped instruction: {:?}", insn);
        } else {
            // N.B. We nop out instructions as requested only *here*, after the
            // checker call, because the checker must observe even elided moves
            // (they may carry useful information about a move between two virtual
            // locations mapped to the same physical location).
            trace!("nop'ing out {:?}", insnIx);
            let nop = func.gen_zero_len_nop();
            let insn = func.get_insn_mut(insnIx);
            *insn = nop;
        }

        // Update cursorStarts and cursorEnds for the next iteration
        cursor_starts += numStarts;
        cursor_ends += numEnds;
    }

    debug_assert!(map.is_empty());

    if use_checker {
        checker.unwrap().run()
    } else {
        Ok(())
    }
}

//=============================================================================
// Take the real-register-only code created by `map_vregs_to_rregs` and
// interleave extra instructions (spills, reloads and moves) that the core
// algorithm has asked us to add.

#[inline(never)]
fn add_spills_reloads_and_moves<F: Function>(
    func: &mut F,
    mut insts_to_add: Vec<InstToInsertAndPoint>,
) -> Result<
    (
        Vec<F::Inst>,
        TypedIxVec<BlockIx, InstIx>,
        TypedIxVec<InstIx, InstIx>,
    ),
    String,
> {
    // Construct the final code by interleaving the mapped code with the the
    // spills, reloads and moves that we have been requested to insert.  To do
    // that requires having the latter sorted by InstPoint.
    //
    // We also need to examine and update Func::blocks.  This is assumed to
    // be arranged in ascending order of the Block::start fields.

    insts_to_add.sort_by_key(|mem_move| mem_move.point);

    let mut curITA = 0; // cursor in `insts_to_add`
    let mut curB = BlockIx::new(0); // cursor in Func::blocks

    let mut insns: Vec<F::Inst> = vec![];
    let mut target_map: TypedIxVec<BlockIx, InstIx> = TypedIxVec::new();
    let mut orig_insn_map: TypedIxVec<InstIx, InstIx> = TypedIxVec::new();
    target_map.reserve(func.blocks().len());
    orig_insn_map.reserve(func.insn_indices().len() + insts_to_add.len());

    for iix in func.insn_indices() {
        // Is `iix` the first instruction in a block?  Meaning, are we
        // starting a new block?
        debug_assert!(curB.get() < func.blocks().len() as u32);
        if func.block_insns(curB).start() == iix {
            assert!(curB.get() == target_map.len());
            target_map.push(InstIx::new(insns.len() as u32));
        }

        // Copy to the output vector, the extra insts that are to be placed at the
        // reload point of `iix`.
        while curITA < insts_to_add.len()
            && insts_to_add[curITA].point == InstPoint::new_reload(iix)
        {
            insns.push(insts_to_add[curITA].inst.construct(func));
            orig_insn_map.push(InstIx::invalid_value());
            curITA += 1;
        }
        // Copy the inst at `iix` itself
        orig_insn_map.push(iix);
        insns.push(func.get_insn(iix).clone());
        // And copy the extra insts that are to be placed at the spill point of
        // `iix`.
        while curITA < insts_to_add.len() && insts_to_add[curITA].point == InstPoint::new_spill(iix)
        {
            insns.push(insts_to_add[curITA].inst.construct(func));
            orig_insn_map.push(InstIx::invalid_value());
            curITA += 1;
        }

        // Is `iix` the last instruction in a block?
        if iix == func.block_insns(curB).last() {
            debug_assert!(curB.get() < func.blocks().len() as u32);
            curB = curB.plus(1);
        }
    }

    debug_assert!(curITA == insts_to_add.len());
    debug_assert!(curB.get() == func.blocks().len() as u32);

    Ok((insns, target_map, orig_insn_map))
}

//=============================================================================
// Main function

#[inline(never)]
pub(crate) fn edit_inst_stream<F: Function>(
    func: &mut F,
    insts_to_add: Vec<InstToInsertAndPoint>,
    iixs_to_nop_out: &Vec<InstIx>,
    frag_map: Vec<(RangeFragIx, VirtualReg, RealReg)>,
    frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
    reg_universe: &RealRegUniverse,
    has_multiple_blocks_per_frag: bool,
    use_checker: bool,
) -> Result<
    (
        Vec<F::Inst>,
        TypedIxVec<BlockIx, InstIx>,
        TypedIxVec<InstIx, InstIx>,
    ),
    RegAllocError,
> {
    map_vregs_to_rregs(
        func,
        frag_map,
        frag_env,
        &insts_to_add,
        iixs_to_nop_out,
        reg_universe,
        has_multiple_blocks_per_frag,
        use_checker,
    )
    .map_err(|e| RegAllocError::RegChecker(e))?;
    add_spills_reloads_and_moves(func, insts_to_add).map_err(|e| RegAllocError::Other(e))
}
