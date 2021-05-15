#![no_main]
use libfuzzer_sys::fuzz_target;

use minira::{self, test_framework as ir};
use regalloc;

// This is the number of test cases the fuzzing framework has given to us so
// far.  More then half of these get rejected as having unreachable blocks, or
// critical edges, or live values into the start node, or for whatever reason
// they are invalid.  Hence ..
static mut COUNTER_GEN: usize = 0;

// .. this is used to count the number of test cases which actually made it
// through the allocator.  This number gives a better measure of the extent of
// test coverage.
static mut COUNTER_OK: usize = 0;

fuzz_target!(|func: ir::Func| {
    let n_gen = unsafe {
        COUNTER_GEN += 1;
        COUNTER_GEN
    };
    let n_ok = unsafe { COUNTER_OK };
    println!(
        "==== BEGIN fuzz_bt.rs: #gen'd {:?} #ok {} ========================",
        n_gen, n_ok
    );

    if false {
        println!("BEGIN INPUT:");
        let mut rendered = String::new();
        func.render("==== fuzz_bt.rs: input:", &mut rendered)
            .unwrap();
        println!("{}", rendered);
        println!("END INPUT:");
    }

    let mut func = func;

    let num_regs = minira::fuzzing::NUM_REAL_REGS_PER_RC as usize;
    let reg_universe = ir::make_universe(num_regs, num_regs);

    let func_backup = func.clone();

    let opts = regalloc::Options {
        run_checker: true,

        algorithm: regalloc::Algorithm::Backtracking(Default::default()),
    };

    let sri = func.get_stackmap_request();
    let bump = regalloc::Bump::new();
    let alloc = regalloc::Alloc(&bump);
    let ra_result = regalloc::allocate_registers_with_opts(
        &mut func,
        &reg_universe,
        sri.as_ref(),
        opts,
        &alloc,
    );

    match ra_result {
        Ok(result) => {
            func.update_from_alloc(result);
            unsafe {
                COUNTER_OK += 1;
            }
            return;
        }
        Err(err) => {
            let mut stop = false;
            if let regalloc::RegAllocError::RegChecker(_) = &err {
                stop = true;
                println!("==== fuzz_bt.rs: checker error: {:?}", err);
            }
            if stop {
                let mut rendered = String::new();
                func_backup
                    .render("==== fuzz_bt.rs: failing input:", &mut rendered)
                    .unwrap();
                println!("{}", rendered);
            }
            println!("==== fuzz_bt.rs: failure reason: {}", err);
            if stop {
                println!("==== fuzz_bt.rs:");
                println!(
                    "==== fuzz_bt.rs: to repro, use flags '-f {} -i {}'",
                    num_regs, num_regs
                );
                println!("==== fuzz_bt.rs:");
                panic!("==== fuzz_bt.rs: STOPPING.  Bye! ====");
            }
        }
    };
});
