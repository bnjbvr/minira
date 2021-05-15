#![no_main]
use libfuzzer_sys::fuzz_target;

use minira::{self, test_framework as ir};
use regalloc;

static mut COUNTER_GEN: usize = 0;
static mut COUNTER_OK: usize = 0;

fuzz_target!(|func: ir::Func| {
    let (num_gen, num_ok) = unsafe {
        COUNTER_GEN += 1;
        (COUNTER_GEN, COUNTER_OK)
    };

    println!(
        "=== status: #ok/#total: {}/{} == {} ",
        num_ok,
        num_gen,
        100.0 * (num_ok as f64) / (num_gen as f64)
    );

    let mut func = func;
    let original_func = func.clone();

    // Add one register to be used as a scratch.
    let num_regs = minira::fuzzing::NUM_REAL_REGS_PER_RC as usize + 1;
    let reg_universe = ir::make_universe(num_regs, num_regs);

    let sri = func.get_stackmap_request();
    let bump = regalloc::Bump::new();
    let alloc = regalloc::Alloc(&bump);
    let result = match regalloc::allocate_registers_with_opts(
        &mut func,
        &reg_universe,
        sri.as_ref(),
        regalloc::Options {
            run_checker: true,
            algorithm: regalloc::Algorithm::LinearScan(Default::default()),
        },
        &alloc,
    ) {
        Ok(result) => {
            unsafe {
                COUNTER_OK += 1;
            }
            result
        }
        Err(err) => {
            if let regalloc::RegAllocError::RegChecker(_) = &err {
                let mut rendered = String::new();
                original_func.render("func:", &mut rendered).unwrap();
                println!("{}", rendered);

                panic!(format!(
                    "fuzz_lsra_differential.rs: checker error: {:?}",
                    err
                ));
            }

            println!("allocation error: {}", err);
            return;
        }
    };

    func.update_from_alloc(result);
});
