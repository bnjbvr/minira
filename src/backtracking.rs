/* -*- Mode: Rust; tab-width: 8; indent-tabs-mode: nil; rust-indent-offset: 2 -*-
 * vim: set ts=8 sts=2 et sw=2 tw=80:
*/

#![allow(non_snake_case)]
#![allow(unused_imports)]
#![allow(non_camel_case_types)]

//! Core implementation of the backtracking allocator.

use crate::analysis::run_analysis;
use crate::data_structures::{
  mkBlockIx, mkInstIx, mkInstPoint, mkRangeFrag, mkRangeFragIx, mkRealReg,
  mkSpillSlot, mkVirtualRangeIx, BlockIx, InstIx, InstPoint, InstPoint_Def,
  InstPoint_Reload, InstPoint_Spill, InstPoint_Use, Map, Point, RangeFrag,
  RangeFragIx, RangeFragKind, RealRange, RealReg, RealRegUniverse, Reg,
  RegClass, Set, SortedRangeFragIxs, SpillSlot, TypedIxVec, VirtualRange,
  VirtualRangeIx, VirtualReg,
};
use crate::interface::{Function, RegAllocResult};
use log::debug;
use std::fmt;

//=============================================================================
// The as-yet-unallocated VirtualReg LR prio queue
//
// Relevant methods are parameterised by a VirtualRange env.

struct VirtualRangePrioQ {
  // The set of as-yet unallocated VirtualRanges.  These are indexes into a
  // VirtualRange env that is not stored here.  These indices can come and
  // go.
  unallocated: Vec<VirtualRangeIx>,
}
impl VirtualRangePrioQ {
  fn new(vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>) -> Self {
    let mut res = Self { unallocated: Vec::new() };
    for vlrix in mkVirtualRangeIx(0).dotdot(mkVirtualRangeIx(vlr_env.len())) {
      assert!(vlr_env[vlrix].size > 0);
      res.unallocated.push(vlrix);
    }
    res
  }

  #[inline(never)]
  fn is_empty(&self) -> bool {
    self.unallocated.is_empty()
  }

  // Add a VirtualRange index.
  #[inline(never)]
  fn add_VirtualRange(&mut self, vlr_ix: VirtualRangeIx) {
    self.unallocated.push(vlr_ix);
  }

  // Look in |unallocated| to locate the entry referencing the VirtualRange
  // with the largest |size| value.  Remove the ref from |unallocated| and
  // return the LRIx for said entry.
  #[inline(never)]
  fn get_longest_VirtualRange(
    &mut self, vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
  ) -> Option<VirtualRangeIx> {
    if self.unallocated.len() == 0 {
      return None;
    }
    let mut largestIx = self.unallocated.len(); /*INVALID*/
    let mut largestSize = 0; /*INVALID*/
    for i in 0..self.unallocated.len() {
      let cand_vlrix = self.unallocated[i];
      let cand_vlr = &vlr_env[cand_vlrix];
      if cand_vlr.size > largestSize {
        largestSize = cand_vlr.size;
        largestIx = i;
      }
    }
    // We must have found *something* since there was at least one
    // unallocated VirtualRange still available.
    debug_assert!(largestIx < self.unallocated.len());
    debug_assert!(largestSize > 0);
    // What we will return
    let res = self.unallocated[largestIx];
    // Remove the selected |unallocated| entry in constant time.
    self.unallocated[largestIx] = self.unallocated[self.unallocated.len() - 1];
    self.unallocated.remove(self.unallocated.len() - 1);
    Some(res)
  }

  #[inline(never)]
  fn show_with_envs(
    &self, vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
  ) -> Vec<String> {
    let mut resV = vec![];
    for vlrix in self.unallocated.iter() {
      let mut res = "TODO  ".to_string();
      res += &format!("{:?}", &vlr_env[*vlrix]);
      resV.push(res);
    }
    resV
  }
}

//=============================================================================
// The per-real-register state
//
// Relevant methods are expected to be parameterised by the same VirtualRange
// env as used in calls to |VirtualRangePrioQ|.

struct PerRealReg {
  // The current committed fragments for this RealReg.
  frags_in_use: SortedRangeFragIxs,

  // The VirtualRanges which have been assigned to this RealReg, in no
  // particular order.  The union of their frags will be equal to
  // |frags_in_use| only if this RealReg has no RealRanges.  If this RealReg
  // does have RealRanges the aforementioned union will be exactly the
  // subset of |frags_in_use| not used by the RealRanges.
  vlrixs_assigned: Vec<VirtualRangeIx>,
}
impl PerRealReg {
  fn new(fenv: &TypedIxVec<RangeFragIx, RangeFrag>) -> Self {
    Self {
      frags_in_use: SortedRangeFragIxs::new(&vec![], fenv),
      vlrixs_assigned: Vec::<VirtualRangeIx>::new(),
    }
  }

  #[inline(never)]
  fn add_RealRange(
    &mut self, to_add: &RealRange, fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) {
    // Commit this register to |to_add|, irrevocably.  Don't add it to
    // |vlrixs_assigned| since we will never want to later evict the
    // assignment.
    self.frags_in_use.add(&to_add.sortedFrags, fenv);
  }

  #[inline(never)]
  fn can_add_VirtualRange_without_eviction(
    &self, to_add: &VirtualRange, fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) -> bool {
    self.frags_in_use.can_add(&to_add.sortedFrags, fenv)
  }

  #[inline(never)]
  fn add_VirtualRange(
    &mut self, to_add_vlrix: VirtualRangeIx,
    vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
    fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) {
    let vlr = &vlr_env[to_add_vlrix];
    self.frags_in_use.add(&vlr.sortedFrags, fenv);
    self.vlrixs_assigned.push(to_add_vlrix);
  }

  #[inline(never)]
  fn del_VirtualRange(
    &mut self, to_del_vlrix: VirtualRangeIx,
    vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
    fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) {
    debug_assert!(self.vlrixs_assigned.len() > 0);
    // Remove it from |vlrixs_assigned|
    let mut found = None;
    for i in 0..self.vlrixs_assigned.len() {
      if self.vlrixs_assigned[i] == to_del_vlrix {
        found = Some(i);
        break;
      }
    }
    if let Some(i) = found {
      self.vlrixs_assigned[i] =
        self.vlrixs_assigned[self.vlrixs_assigned.len() - 1];
      self.vlrixs_assigned.remove(self.vlrixs_assigned.len() - 1);
    } else {
      panic!("PerRealReg: del_VirtualRange on VR not in vlrixs_assigned");
    }
    // Remove it from |frags_in_use|
    let vlr = &vlr_env[to_del_vlrix];
    self.frags_in_use.del(&vlr.sortedFrags, fenv);
  }

  #[inline(never)]
  fn find_best_evict_VirtualRange(
    &self, would_like_to_add: &VirtualRange,
    vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
    frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) -> Option<(VirtualRangeIx, f32)> {
    // |would_like_to_add| presumably doesn't fit here.  See if eviction
    // of any of the existing LRs would make it allocable, and if so
    // return that LR and its cost.  Valid candidates VirtualRanges must
    // meet the following criteria:
    // - must be assigned to this register (obviously)
    // - must have a non-infinite spill cost
    //   (since we don't want to eject spill/reload LRs)
    // - must have a spill cost less than that of |would_like_to_add|
    //   (so as to guarantee forward progress)
    // - removal of it must actually make |would_like_to_add| allocable
    //   (otherwise all this is pointless)
    let mut best_so_far: Option<(VirtualRangeIx, f32)> = None;
    for cand_vlrix in &self.vlrixs_assigned {
      let cand_vlr = &vlr_env[*cand_vlrix];
      if cand_vlr.spillCost.is_none() {
        continue;
      }
      let cand_cost = cand_vlr.spillCost.unwrap();
      if !cost_is_less(Some(cand_cost), would_like_to_add.spillCost) {
        continue;
      }
      if !self.frags_in_use.can_add_if_we_first_del(
        &cand_vlr.sortedFrags,
        &would_like_to_add.sortedFrags,
        frag_env,
      ) {
        continue;
      }
      // Ok, it's at least a valid candidate.  But is it better than
      // any candidate we might already have?
      let mut cand_is_better = true;
      if let Some((_, best_cost)) = best_so_far {
        if cost_is_less(Some(best_cost), Some(cand_cost)) {
          cand_is_better = false;
        }
      }
      if cand_is_better {
        // Either this is the first possible candidate we've seen, or
        // it's better than any previous one.  In either case, make
        // note of it.
        best_so_far = Some((*cand_vlrix, cand_cost));
      }
    }
    best_so_far
  }

  #[inline(never)]
  fn show1_with_envs(
    &self, fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) -> String {
    "in_use:   ".to_string() + &self.frags_in_use.show_with_fenv(&fenv)
  }
  #[inline(never)]
  fn show2_with_envs(
    &self, _fenv: &TypedIxVec<RangeFragIx, RangeFrag>,
  ) -> String {
    "assigned: ".to_string() + &format!("{:?}", &self.vlrixs_assigned)
  }
}

// Helper function, to compare spill costs
fn cost_is_less(cost1: Option<f32>, cost2: Option<f32>) -> bool {
  // None denotes "infinity", while Some(_) is some number less than
  // infinity.  No matter that the enclosed f32 can denote its own infinity
  // :-/ (they never actually should be infinity, nor negative, nor any
  // denormal either).
  match (cost1, cost2) {
    (None, None) => false,
    (Some(_), None) => true,
    (None, Some(_)) => false,
    (Some(f1), Some(f2)) => f1 < f2,
  }
}

//=============================================================================
// Edit list items

// VirtualRanges created by spilling all pertain to a single InstIx.  But
// within that InstIx, there are three kinds of "bridges":
#[derive(PartialEq)]
enum BridgeKind {
  RtoU, // A bridge for a USE.  This connects the reload to the use.
  RtoS, // a bridge for a MOD.  This connects the reload, the use/def
  // and the spill, since the value must first be reloade, then
  // operated on, and finally re-spilled.
  DtoS, // A bridge for a DEF.  This connects the def to the spill.
}
impl fmt::Debug for BridgeKind {
  fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
    match self {
      BridgeKind::RtoU => write!(fmt, "R->U"),
      BridgeKind::RtoS => write!(fmt, "R->S"),
      BridgeKind::DtoS => write!(fmt, "D->S"),
    }
  }
}

pub(crate) struct EditListItem {
  // This holds enough information to create a spill or reload instruction,
  // or both, and also specifies where in the instruction stream it/they
  // should be added.  Note that if the edit list as a whole specifies
  // multiple items for the same location, then it is assumed that the order
  // in which they execute isn't important.
  //
  // Some of the relevant info can be found via the VirtualRangeIx link:
  // * the real reg involved
  // * the place where the insn should go, since the VirtualRange should only
  //   have one RangeFrag, and we can deduce the correct location from that.
  slot: SpillSlot,
  vlrix: VirtualRangeIx,
  kind: BridgeKind,
}

impl fmt::Debug for EditListItem {
  fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
    write!(
      fmt,
      "ELI {{ for {:?} add '{:?}' {:?} }}",
      self.vlrix, self.kind, self.slot
    )
  }
}

pub(crate) type EditList = Vec<EditListItem>;

//=============================================================================
// Printing the allocator's top level state

#[inline(never)]
fn print_RA_state(
  who: &str,
  universe: &RealRegUniverse,
  // State components
  prioQ: &VirtualRangePrioQ,
  perRealReg: &Vec<PerRealReg>,
  edit_list: &Vec<EditListItem>,
  // The context (environment)
  vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>,
  frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
) {
  debug!("<<<<====---- RA state at '{}' ----====", who);
  for ix in 0..perRealReg.len() {
    debug!(
      "{:<4}   {}",
      universe.regs[ix].1,
      &perRealReg[ix].show1_with_envs(&frag_env)
    );
    debug!("      {}", &perRealReg[ix].show2_with_envs(&frag_env));
    debug!("");
  }
  if !prioQ.is_empty() {
    for s in prioQ.show_with_envs(&vlr_env) {
      debug!("{}", s);
    }
  }
  for eli in edit_list {
    debug!("{:?}", eli);
  }
  debug!(">>>>");
}

//=============================================================================
// Allocator top level

/* (const) For each virtual live range
   - its sorted RangeFrags
   - its total size
   - its spill cost
   - (eventually) its assigned real register
   New VirtualRanges are created as we go, but existing ones are unchanged,
   apart from being tagged with its assigned real register.

   (mut) For each real register
   - the sorted RangeFrags assigned to it (todo: rm the M)
   - the virtual LR indices assigned to it.  This is so we can eject
     a VirtualRange from the commitment, as needed

   (mut) the set of VirtualRanges not yet allocated, prioritised by total size

   (mut) the edit list that is produced
*/
/*
fn show_commit_tab(commit_tab: &Vec::<SortedRangeFragIxs>,
                   who: &str,
                   context: &TypedIxVec::<RangeFragIx, RangeFrag>) -> String {
    let mut res = "Commit Table at '".to_string()
                  + &who.to_string() + &"'\n".to_string();
    let mut rregNo = 0;
    for smf in commit_tab.iter() {
        res += &"  ".to_string();
        res += &mkRealReg(rregNo).show();
        res += &" ".to_string();
        res += &smf.show_with_fenv(&context);
        res += &"\n".to_string();
        rregNo += 1;
    }
    res
}
*/

// Allocator top level.  This function returns a result struct that contains
// the final sequence of instructions, possibly with fills/spills/moves
// spliced in and redundant moves elided, and with all virtual registers
// replaced with real registers. Allocation can fail if there are insufficient
// registers to even generate spill/reload code, or if the function appears to
// have any undefined VirtualReg/RealReg uses.

#[inline(never)]
pub fn alloc_main<F: Function>(
  func: &mut F, reg_universe: &RealRegUniverse,
) -> Result<RegAllocResult<F>, String> {
  // Note that the analysis phase can fail; hence we propagate any error.
  let (rlr_env, mut vlr_env, mut frag_env) = run_analysis(func)?;

  // -------- Alloc main --------

  // Create initial state

  // This is fully populated by the ::new call.
  let mut prioQ = VirtualRangePrioQ::new(&vlr_env);

  // Whereas this is empty.  We have to populate it "by hand", by
  // effectively cloning the allocatable part (prefix) of the universe.
  let mut per_real_reg = Vec::<PerRealReg>::new();
  for _ in 0..reg_universe.allocable {
    // Doing this instead of simply .resize avoids needing Clone for
    // PerRealReg
    per_real_reg.push(PerRealReg::new(&frag_env));
  }
  for rlr in rlr_env.iter() {
    let rregIndex = rlr.rreg.get_index();
    // Ignore RealRanges for RealRegs that are not part of the allocatable
    // set.  As far as the allocator is concerned, such RealRegs simply
    // don't exist.
    if rregIndex >= reg_universe.allocable {
      continue;
    }
    per_real_reg[rregIndex].add_RealRange(&rlr, &frag_env);
  }

  let mut edit_list = Vec::<EditListItem>::new();
  debug!("");
  print_RA_state(
    "Initial",
    &reg_universe,
    &prioQ,
    &per_real_reg,
    &edit_list,
    &vlr_env,
    &frag_env,
  );

  // This is technically part of the running state, at least for now.
  let mut next_spill_slot: SpillSlot = mkSpillSlot(0);

  // Main allocation loop.  Each time round, pull out the longest
  // unallocated VirtualRange, and do one of three things:
  //
  // * allocate it to a RealReg, possibly by ejecting some existing
  //   allocation, but only one with a lower spill cost than this one, or
  //
  // * spill it.  This causes the VirtualRange to disappear.  It is replaced
  //   by a set of very short VirtualRanges to carry the spill and reload
  //  values.  Or,
  //
  // * split it.  This causes it to disappear but be replaced by two
  //   VirtualRanges which together constitute the original.
  debug!("");
  debug!("-- MAIN ALLOCATION LOOP:");
  while let Some(curr_vlrix) = prioQ.get_longest_VirtualRange(&vlr_env) {
    let curr_vlr = &vlr_env[curr_vlrix];

    debug!("-- considering        {:?}:  {:?}", curr_vlrix, curr_vlr);

    debug_assert!(curr_vlr.vreg.to_reg().is_virtual());
    let curr_vlr_rc = curr_vlr.vreg.get_class().rc_to_usize();

    let (first_in_rc, last_in_rc) =
      match reg_universe.allocable_by_class[curr_vlr_rc] {
        None => {
          // Urk.  This is very ungood.  Game over.
          let s = format!(
            "no available registers for class {:?}",
            RegClass::rc_from_u32(curr_vlr_rc as u32)
          );
          return Err(s);
        }
        Some((first, last)) => (first, last),
      };

    // See if we can find a RealReg to which we can assign this
    // VirtualRange without evicting any previous assignment.
    let mut rreg_to_use = None;
    for i in first_in_rc..last_in_rc + 1 {
      if per_real_reg[i]
        .can_add_VirtualRange_without_eviction(curr_vlr, &frag_env)
      {
        rreg_to_use = Some(i);
        break;
      }
    }
    if let Some(rregNo) = rreg_to_use {
      // Yay!
      let rreg = reg_universe.regs[rregNo].0;
      debug!("--   direct alloc to  {:?}", rreg);
      per_real_reg[rregNo].add_VirtualRange(curr_vlrix, &vlr_env, &frag_env);
      debug_assert!(curr_vlr.rreg.is_none());
      // Directly modify bits of vlr_env.  This means we have to abandon
      // the immutable borrow for curr_vlr, but that's OK -- we won't
      // need it again (in this loop iteration).
      vlr_env[curr_vlrix].rreg = Some(rreg);
      continue;
    }

    // That didn't work out.  Next, try to see if we can allocate it by
    // evicting some existing allocation that has a lower spill weight.
    // Search all RealRegs to find the candidate with the lowest spill
    // weight.  This could be expensive.

    // (rregNo for best cand, its VirtualRangeIx, and its spill cost)
    let mut best_so_far: Option<(usize, VirtualRangeIx, f32)> = None;
    for i in first_in_rc..last_in_rc + 1 {
      let mb_better_cand: Option<(VirtualRangeIx, f32)>;
      mb_better_cand = per_real_reg[i]
        .find_best_evict_VirtualRange(&curr_vlr, &vlr_env, &frag_env);
      if let Some((cand_vlrix, cand_cost)) = mb_better_cand {
        // See if this is better than the best so far, and if so take
        // it.  First some sanity checks:
        //
        // If the cand to be evicted is the current one,
        // something's seriously wrong.
        debug_assert!(cand_vlrix != curr_vlrix);
        // We can only evict a LR with lower spill cost than the LR
        // we're trying to allocate.  This is really important, if only
        // to show that the algorithm will actually terminate.
        debug_assert!(cost_is_less(Some(cand_cost), curr_vlr.spillCost));
        let mut cand_is_better = true;
        if let Some((_, _, best_cost)) = best_so_far {
          if cost_is_less(Some(best_cost), Some(cand_cost)) {
            cand_is_better = false;
          }
        }
        if cand_is_better {
          // Either this is the first possible candidate we've seen,
          // or it's better than any previous one.  In either case,
          // make note of it.
          best_so_far = Some((i, cand_vlrix, cand_cost));
        }
      }
    }
    if let Some((rregNo, vlrix_to_evict, _)) = best_so_far {
      // Evict ..
      debug!(
        "--   evict            {:?}:  {:?}",
        vlrix_to_evict, &vlr_env[vlrix_to_evict]
      );
      debug_assert!(vlrix_to_evict != curr_vlrix);
      per_real_reg[rregNo].del_VirtualRange(
        vlrix_to_evict,
        &vlr_env,
        &frag_env,
      );
      prioQ.add_VirtualRange(vlrix_to_evict);
      debug_assert!(vlr_env[vlrix_to_evict].rreg.is_some());
      // .. and reassign.
      let rreg = reg_universe.regs[rregNo].0;
      debug!("--   then alloc to    {:?}", rreg);
      per_real_reg[rregNo].add_VirtualRange(curr_vlrix, &vlr_env, &frag_env);
      debug_assert!(curr_vlr.rreg.is_none());
      // Directly modify bits of vlr_env.  This means we have to abandon
      // the immutable borrow for curr_vlr, but that's OK -- we won't
      // need it again again (in this loop iteration).
      vlr_env[curr_vlrix].rreg = Some(rreg);
      vlr_env[vlrix_to_evict].rreg = None;
      continue;
    }

    // Still no luck.  We can't find a register to put it in, so we'll
    // have to spill it, since splitting it isn't yet implemented.
    debug!("--   spill");

    // If the live range already pertains to a spill or restore, then
    // it's Game Over.  The constraints (availability of RealRegs vs
    // requirement for them) are impossible to fulfill, and so we cannot
    // generate any valid allocation for this function.
    if curr_vlr.spillCost.is_none() {
      return Err("out of registers".to_string());
    }

    // Generate a new spill slot number, S
    /* Spilling vreg V with virtual live range VirtualRange to slot S:
          for each frag F in VirtualRange {
             for each insn I in F.first.iix .. F.last.iix {
                if I does not mention V
                   continue
                if I mentions V in a Read role {
                   // invar: I cannot mention V in a Mod role
                   add new VirtualRange I.reload -> I.use,
                                        vreg V, spillcost Inf
                   add eli @ I.reload V SpillSlot
                }
                if I mentions V in a Mod role {
                   // invar: I cannot mention V in a Read or Write Role
                   add new VirtualRange I.reload -> I.spill,
                                        Vreg V, spillcost Inf
                   add eli @ I.reload V SpillSlot
                   add eli @ I.spill  SpillSlot V
                }
                if I mentions V in a Write role {
                   // invar: I cannot mention V in a Mod role
                   add new VirtualRange I.def -> I.spill,
                                        vreg V, spillcost Inf
                   add eli @ I.spill V SpillSlot
                }
             }
          }
    */
    /* We will be spilling vreg |curr_vlr.reg| with VirtualRange
    |curr_vlr| to ..  well, we better invent a new spill slot number.
    Just hand them out sequentially for now. */

    // This holds enough info to create reload or spill (or both)
    // instructions around an instruction that references a VirtualReg
    // that has been spilled.
    struct SpillAndOrReloadInfo {
      bix: BlockIx,     // that |iix| is in
      iix: InstIx,      // this is the Inst we are spilling/reloading for
      kind: BridgeKind, // says whether to create a spill or reload or both
    }
    let mut sri_vec = Vec::<SpillAndOrReloadInfo>::new();
    let curr_vlr_vreg = curr_vlr.vreg;
    let curr_vlr_class = curr_vlr_vreg.get_class();
    let curr_vlr_reg = curr_vlr_vreg.to_reg();

    for fix in &curr_vlr.sortedFrags.fragIxs {
      let frag: &RangeFrag = &frag_env[*fix];
      for iix in frag.first.iix.dotdot(frag.last.iix.plus(1)) {
        let reg_usage = func.get_regs(func.get_insn(iix));
        // If this insn doesn't mention the vreg we're spilling for,
        // move on.
        if !reg_usage.defined.contains(curr_vlr_reg)
          && !reg_usage.modified.contains(curr_vlr_reg)
          && !reg_usage.used.contains(curr_vlr_reg)
        {
          continue;
        }
        // USES: Do we need to create a reload-to-use bridge
        // (VirtualRange) ?
        if reg_usage.used.contains(curr_vlr_reg)
          && frag.contains(&InstPoint_Use(iix))
        {
          debug_assert!(!reg_usage.modified.contains(curr_vlr_reg));
          // Stash enough info that we can create a new VirtualRange
          // and a new edit list entry for the reload.
          let sri =
            SpillAndOrReloadInfo { bix: frag.bix, iix, kind: BridgeKind::RtoU };
          sri_vec.push(sri);
        }
        // MODS: Do we need to create a reload-to-spill bridge?  This
        // can only happen for instructions which modify a register.
        // Note this has to be a single VirtualRange, since if it were
        // two (one for the reload, one for the spill) they could
        // later end up being assigned to different RealRegs, which is
        // obviously nonsensical.
        if reg_usage.modified.contains(curr_vlr_reg)
          && frag.contains(&InstPoint_Use(iix))
          && frag.contains(&InstPoint_Def(iix))
        {
          debug_assert!(!reg_usage.used.contains(curr_vlr_reg));
          debug_assert!(!reg_usage.defined.contains(curr_vlr_reg));
          let sri =
            SpillAndOrReloadInfo { bix: frag.bix, iix, kind: BridgeKind::RtoS };
          sri_vec.push(sri);
        }
        // DEFS: Do we need to create a def-to-spill bridge?
        if reg_usage.defined.contains(curr_vlr_reg)
          && frag.contains(&InstPoint_Def(iix))
        {
          debug_assert!(!reg_usage.modified.contains(curr_vlr_reg));
          let sri =
            SpillAndOrReloadInfo { bix: frag.bix, iix, kind: BridgeKind::DtoS };
          sri_vec.push(sri);
        }
      }
    }

    // Now that we no longer need to access |frag_env| or |vlr_env| for
    // the remainder of this iteration of the main allocation loop, we can
    // actually generate the required spill/reload artefacts.
    let num_slots = func.get_spillslot_size(curr_vlr_class);
    next_spill_slot = next_spill_slot.round_up(num_slots);
    for sri in sri_vec {
      // For a spill for a MOD use, the new value will be referenced
      // three times.  For DEF and USE uses, it'll only be ref'd twice.
      // (I think we don't care about metrics for the new RangeFrags,
      // though)
      let new_vlr_count = if sri.kind == BridgeKind::RtoS { 3 } else { 2 };
      let (new_vlr_first_pt, new_vlr_last_pt) = match sri.kind {
        BridgeKind::RtoU => (Point::Reload, Point::Use),
        BridgeKind::RtoS => (Point::Reload, Point::Spill),
        BridgeKind::DtoS => (Point::Def, Point::Spill),
      };
      let new_vlr_frag = RangeFrag {
        bix: sri.bix,
        kind: RangeFragKind::Local,
        first: mkInstPoint(sri.iix, new_vlr_first_pt),
        last: mkInstPoint(sri.iix, new_vlr_last_pt),
        count: new_vlr_count,
      };
      let new_vlr_fix = mkRangeFragIx(frag_env.len() as u32);
      frag_env.push(new_vlr_frag);
      debug!(
        "--     new RangeFrag       {:?}  :=  {:?}",
        &new_vlr_fix, &new_vlr_frag
      );
      let new_vlr_sfixs = SortedRangeFragIxs::unit(new_vlr_fix, &frag_env);
      let new_vlr = VirtualRange {
        vreg: curr_vlr_vreg,
        rreg: None,
        sortedFrags: new_vlr_sfixs,
        size: 1,
        spillCost: None, /*infinity*/
      };
      let new_vlrix = mkVirtualRangeIx(vlr_env.len() as u32);
      debug!(
        "--     new VirtualRange        {:?}  :=  {:?}",
        new_vlrix, &new_vlr
      );
      vlr_env.push(new_vlr);
      prioQ.add_VirtualRange(new_vlrix);

      let new_eli = EditListItem {
        slot: next_spill_slot,
        vlrix: new_vlrix,
        kind: sri.kind,
      };
      debug!("--     new ELI        {:?}", &new_eli);
      edit_list.push(new_eli);
    }

    next_spill_slot = next_spill_slot.inc(num_slots);
  }

  debug!("");
  print_RA_state(
    "Final",
    &reg_universe,
    &prioQ,
    &per_real_reg,
    &edit_list,
    &vlr_env,
    &frag_env,
  );

  // Gather up a vector of (RangeFrag, VirtualReg, RealReg) resulting from
  // the previous phase.  This fundamentally is the result of the allocation
  // and tells us how the instruction stream must be edited.  Note it does
  // not take account of spill or reload instructions.  Dealing with those
  // is relatively simple and happens later.
  //
  // Make two copies of this list, one sorted by the fragment start points
  // (just the InstIx numbers, ignoring the Point), and one sorted by
  // fragment end points.

  let mut frag_maps_by_start = RangeAllocations::new();
  let mut frag_maps_by_end = RangeAllocations::new();
  // For each real register under our control ..
  for i in 0..reg_universe.allocable {
    let rreg = reg_universe.regs[i].0;
    // .. look at all the VirtualRanges assigned to it.  And for each such
    // VirtualRange ..
    for vlrix_assigned in &per_real_reg[i].vlrixs_assigned {
      let vlr_assigned = &vlr_env[*vlrix_assigned];
      // All the RangeFrags in |vlr_assigned| require |vlr_assigned.reg|
      // to be mapped to the real reg |i|
      let vreg = vlr_assigned.vreg;
      // .. collect up all its constituent RangeFrags.
      for fix in &vlr_assigned.sortedFrags.fragIxs {
        frag_maps_by_start.push((*fix, vreg, rreg));
        frag_maps_by_end.push((*fix, vreg, rreg));
      }
    }
  }

  edit_inst_stream(
    func,
    edit_list,
    frag_maps_by_start,
    frag_maps_by_end,
    &frag_env,
    &vlr_env,
    next_spill_slot.get(),
  )
}

pub(crate) type RangeAllocations = Vec<(RangeFragIx, VirtualReg, RealReg)>;

pub(crate) fn edit_inst_stream<F: Function>(
  func: &mut F, edit_list: EditList, mut frag_maps_by_start: RangeAllocations,
  mut frag_maps_by_end: RangeAllocations,
  frag_env: &TypedIxVec<RangeFragIx, RangeFrag>,
  vlr_env: &TypedIxVec<VirtualRangeIx, VirtualRange>, num_spill_slots: u32,
) -> Result<RegAllocResult<F>, String> {
  // -------- Edit the instruction stream --------
  frag_maps_by_start.sort_unstable_by(
    |(frag_ix, _, _), (other_frag_ix, _, _)| {
      frag_env[*frag_ix]
        .first
        .iix
        .partial_cmp(&frag_env[*other_frag_ix].first.iix)
        .unwrap()
    },
  );

  frag_maps_by_end.sort_unstable_by(
    |(frag_ix, _, _), (other_frag_ix, _, _)| {
      frag_env[*frag_ix]
        .last
        .iix
        .partial_cmp(&frag_env[*other_frag_ix].last.iix)
        .unwrap()
    },
  );

  //debug!("Firsts: {}", fragMapsByStart.show());
  //debug!("Lasts:  {}", fragMapsByEnd.show());

  let mut cursor_starts = 0;
  let mut cursor_ends = 0;

  let mut map = Map::<VirtualReg, RealReg>::default();

  //fn showMap(m: &Map<VirtualReg, RealReg>) -> String {
  //  let mut vec: Vec<(&VirtualReg, &RealReg)> = m.iter().collect();
  //  vec.sort_by(|p1, p2| p1.0.partial_cmp(p2.0).unwrap());
  //  format!("{:?}", vec)
  //}

  fn is_sane(frag: &RangeFrag) -> bool {
    // "Normal" frag (unrelated to spilling).  No normal frag may start or
    // end at a .s or a .r point.
    if frag.first.pt.isUseOrDef()
      && frag.last.pt.isUseOrDef()
      && frag.first.iix <= frag.last.iix
    {
      return true;
    }
    // A spill-related ("bridge") frag.  There are three possibilities,
    // and they correspond exactly to |BridgeKind|.
    if frag.first.pt.isReload()
      && frag.last.pt.isUse()
      && frag.last.iix == frag.first.iix
    {
      // BridgeKind::RtoU
      return true;
    }
    if frag.first.pt.isReload()
      && frag.last.pt.isSpill()
      && frag.last.iix == frag.first.iix
    {
      // BridgeKind::RtoS
      return true;
    }
    if frag.first.pt.isDef()
      && frag.last.pt.isSpill()
      && frag.last.iix == frag.first.iix
    {
      // BridgeKind::DtoS
      return true;
    }
    // None of the above apply.  This RangeFrag is insane \o/
    false
  }

  for insnIx in func.insn_indices() {
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
      && frag_env[frag_maps_by_start[cursor_starts + numStarts].0].first.iix
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
    //   mapU := map
    //   remove frags ending at I.u
    // Update map for I.d:
    //   add frags starting at I.d
    //   mapD := map
    //   remove frags ending at I.d
    // Update map for I.s:
    //   no frags should start at I.s (it's a spill insn)
    //   remove frags ending at I.s
    // apply mapU/mapD to I

    //debug!("QQQQ mapping insn {:?}", insnIx);
    //debug!("QQQQ current map {}", showMap(&map));

    // Update map for I.r:
    //   add frags starting at I.r
    //   no frags should end at I.r (it's a reload insn)
    for j in cursor_starts..cursor_starts + numStarts {
      let frag = &frag_env[frag_maps_by_start[j].0];
      if frag.first.pt.isReload() {
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
    //   mapU := map
    //   remove frags ending at I.u
    for j in cursor_starts..cursor_starts + numStarts {
      let frag = &frag_env[frag_maps_by_start[j].0];
      if frag.first.pt.isUse() {
        //////// STARTS at I.u
        map.insert(frag_maps_by_start[j].1, frag_maps_by_start[j].2);
        //debug!(
        //  "QQQQ inserted frag from use: {:?} -> {:?}",
        //  fragMapsByStart[j].1, fragMapsByStart[j].2
        //);
      }
    }
    let mapU = map.clone();
    for j in cursor_ends..cursor_ends + numEnds {
      let frag = &frag_env[frag_maps_by_end[j].0];
      if frag.last.pt.isUse() {
        //////// ENDS at I.U
        map.remove(&frag_maps_by_end[j].1);
        //debug!("QQQQ removed frag after use: {:?}", fragMapsByStart[j].1);
      }
    }

    // Update map for I.d:
    //   add frags starting at I.d
    //   mapD := map
    //   remove frags ending at I.d
    for j in cursor_starts..cursor_starts + numStarts {
      let frag = &frag_env[frag_maps_by_start[j].0];
      if frag.first.pt.isDef() {
        //////// STARTS at I.d
        map.insert(frag_maps_by_start[j].1, frag_maps_by_start[j].2);
        //debug!(
        //  "QQQQ inserted frag from def: {:?} -> {:?}",
        //  fragMapsByStart[j].1, fragMapsByStart[j].2
        //);
      }
    }
    let mapD = map.clone();
    for j in cursor_ends..cursor_ends + numEnds {
      let frag = &frag_env[frag_maps_by_end[j].0];
      if frag.last.pt.isDef() {
        //////// ENDS at I.d
        map.remove(&frag_maps_by_end[j].1);
        //debug!("QQQQ ended frag from def: {:?}", fragMapsByEnd[j].1);
      }
    }

    // Update map for I.s:
    //   no frags should start at I.s (it's a spill insn)
    //   remove frags ending at I.s
    for j in cursor_ends..cursor_ends + numEnds {
      let frag = &frag_env[frag_maps_by_end[j].0];
      if frag.last.pt.isSpill() {
        //////// ENDS at I.s
        map.remove(&frag_maps_by_end[j].1);
        //debug!("QQQQ ended frag from spill: {:?}", fragMapsByEnd[j].1);
      }
    }

    //debug!("QQQQ mapU {}", showMap(&mapU));
    //debug!("QQQQ mapD {}", showMap(&mapD));

    // Finally, we have mapU/mapD set correctly for this instruction.
    // Apply it.
    let mut insn = func.get_insn_mut(insnIx);
    F::map_regs(&mut insn, &mapU, &mapD);

    // Update cursorStarts and cursorEnds for the next iteration
    cursor_starts += numStarts;
    cursor_ends += numEnds;

    for b in func.blocks() {
      if func.block_insns(b).last() == insnIx {
        //debug!("Block end");
        debug_assert!(map.is_empty());
      }
    }
  }

  debug_assert!(map.is_empty());

  // At this point, we've successfully pushed the vreg->rreg assignments
  // into the original instructions.  But the reload and spill instructions
  // are missing.  To generate them, go through the "edit list", which
  // contains info on both how to generate the instructions, and where to
  // insert them.
  let mut spills_and_reloads = Vec::<(InstPoint, F::Inst)>::new();
  for eli in &edit_list {
    debug!("editlist entry: {:?}", eli);
    let vlr = &vlr_env[eli.vlrix];
    let vlr_sfrags = &vlr.sortedFrags;
    debug_assert!(vlr.sortedFrags.fragIxs.len() == 1);
    let vlr_frag = frag_env[vlr_sfrags.fragIxs[0]];
    let rreg = vlr.rreg.expect("Gen of spill/reload: reg not assigned?!");
    match eli.kind {
      BridgeKind::RtoU => {
        debug_assert!(vlr_frag.first.pt.isReload());
        debug_assert!(vlr_frag.last.pt.isUse());
        debug_assert!(vlr_frag.first.iix == vlr_frag.last.iix);
        let insnR = func.gen_reload(rreg, eli.slot);
        let whereToR = vlr_frag.first;
        spills_and_reloads.push((whereToR, insnR));
      }
      BridgeKind::RtoS => {
        debug_assert!(vlr_frag.first.pt.isReload());
        debug_assert!(vlr_frag.last.pt.isSpill());
        debug_assert!(vlr_frag.first.iix == vlr_frag.last.iix);
        let insnR = func.gen_reload(rreg, eli.slot);
        let whereToR = vlr_frag.first;
        let insnS = func.gen_spill(eli.slot, rreg);
        let whereToS = vlr_frag.last;
        spills_and_reloads.push((whereToR, insnR));
        spills_and_reloads.push((whereToS, insnS));
      }
      BridgeKind::DtoS => {
        debug_assert!(vlr_frag.first.pt.isDef());
        debug_assert!(vlr_frag.last.pt.isSpill());
        debug_assert!(vlr_frag.first.iix == vlr_frag.last.iix);
        let insnS = func.gen_spill(eli.slot, rreg);
        let whereToS = vlr_frag.last;
        spills_and_reloads.push((whereToS, insnS));
      }
    }
  }

  //for pair in &spillsAndReloads {
  //    debug!("spill/reload: {}", pair.show());
  //}

  // Construct the final code by interleaving the mapped code with the
  // spills and reloads.  To do that requires having the latter sorted by
  // InstPoint.
  //
  // We also need to examine and update Func::blocks.  This is assumed to
  // be arranged in ascending order of the Block::start fields.

  spills_and_reloads
    .sort_unstable_by(|(ip1, _), (ip2, _)| ip1.partial_cmp(ip2).unwrap());

  let mut curSnR = 0; // cursor in |spillsAndReloads|
  let mut curB = mkBlockIx(0); // cursor in Func::blocks

  let mut insns: Vec<F::Inst> = vec![];
  let mut target_map: TypedIxVec<BlockIx, InstIx> = TypedIxVec::new();
  let mut clobbered_registers: Set<RealReg> = Set::empty();

  for iix in func.insn_indices() {
    // Is |iix| the first instruction in a block?  Meaning, are we
    // starting a new block?
    debug_assert!(curB.get() < func.blocks().len() as u32);
    if func.block_insns(curB).start() == iix {
      assert!(curB.get() == target_map.len());
      target_map.push(mkInstIx(insns.len() as u32));
    }

    // Copy reloads for this insn
    while curSnR < spills_and_reloads.len()
      && spills_and_reloads[curSnR].0 == InstPoint_Reload(iix)
    {
      insns.push(spills_and_reloads[curSnR].1.clone());
      curSnR += 1;
    }
    // And the insn itself
    insns.push(func.get_insn(iix).clone());
    // Copy spills for this insn
    while curSnR < spills_and_reloads.len()
      && spills_and_reloads[curSnR].0 == InstPoint_Spill(iix)
    {
      insns.push(spills_and_reloads[curSnR].1.clone());
      curSnR += 1;
    }

    // Is |iix| the last instruction in a block?
    if iix == func.block_insns(curB).last() {
      debug_assert!(curB.get() < func.blocks().len() as u32);
      curB = curB.plus(1);
    }
  }

  debug_assert!(curSnR == spills_and_reloads.len());
  debug_assert!(curB.get() == func.blocks().len() as u32);

  // Compute clobbered registers with one final, quick pass.
  //
  // TODO: derive this information directly from the allocation data
  // structures used above.
  for insn in insns.iter() {
    let reg_usage = func.get_regs(insn);
    for reg in reg_usage.modified.iter().chain(reg_usage.defined.iter()) {
      assert!(reg.is_real());
      clobbered_registers.insert(reg.to_real_reg());
    }
  }

  // And we're done!

  Ok(RegAllocResult { insns, target_map, clobbered_registers, num_spill_slots })
}
