mod fuzzing;
mod parser;
mod test_cases;
mod test_framework;
mod validator;

use regalloc::{
    allocate_registers_with_opts, Algorithm, Alloc, BacktrackingOptions, Bump, IRSnapshot, Options,
};
use test_framework::{make_universe, run_func, RunStage};
use validator::check_results;

use clap;
use log::{self, error};
use pretty_env_logger;

//=============================================================================
// Top level

fn main() {
    pretty_env_logger::init();

    let app = clap::App::new("minira")
        .about("a simple program to allow separate testing of regalloc.rs")
        .arg(
            clap::Arg::with_name("iregs")
                .short("i")
                .takes_value(true)
                .help("number of integer registers available (0 if not set)"),
        )
        .arg(
            clap::Arg::with_name("fregs")
                .short("f")
                .takes_value(true)
                .help("number of floating-point registers available (0 if not set)"),
        )
        .arg(
            clap::Arg::with_name("test")
                .short("t")
                .takes_value(true)
                .help("test case name"),
        )
        .arg(
            clap::Arg::with_name("algorithm")
                .short("a")
                .takes_value(true)
                .required(true)
                .possible_values(&["bt", "lsra", "btc", "lsrac"])
                .help("algorithm name"),
        )
        .arg(
            clap::Arg::with_name("quiet")
            .short("q")
            .takes_value(false)
            .help("whether to run in quiet mode (i.e. not print the function's body before and after regalloc)"))
        .arg(
            clap::Arg::with_name("snapshot")
            .short("s")
            .takes_value(true)
            .help("Path to a snapshot file (.bin) or directory containing snapshot files."))
        .arg(
            clap::Arg::with_name("run-snapshot")
            .short("r")
            .takes_value(false)
            .help("Rerun the whole program as a snapshotted test.")
            );

    let matches = app.get_matches();
    if matches.value_of("snapshot").is_none() && matches.value_of("test").is_none() {
        println!("Missing test or snapshot parameter, aborting.");
        println!("Usage: {}", matches.usage());
        return;
    }

    let quiet = matches.is_present("quiet");

    // Find what the algorithm is going to be.
    let algorithm = matches.value_of("algorithm").unwrap();
    let opts = match algorithm {
        "bt" | "btc" => Options {
            run_checker: algorithm == "btc",
            algorithm: Algorithm::Backtracking(BacktrackingOptions {
                request_block_annotations: true,
            }),
        },
        "lsra" | "lsrac" => Options {
            run_checker: algorithm == "lsrac",
            algorithm: Algorithm::LinearScan(Default::default()),
        },
        // Unreachable because of defined "possible_values".
        _ => unreachable!(),
    };

    if let Some(snapshot_path) = matches.value_of("snapshot") {
        return run_snapshot(snapshot_path, opts, quiet);
    }

    let func_name = matches.value_of("test").unwrap();
    let mut func = match crate::test_cases::find_func(func_name) {
        Ok(func) => func,
        Err(available_func_names) => {
            error!("can't find Func with name '{}'", func_name);
            println!("available func names are:");
            for name in available_func_names {
                println!("     {}", name);
            }
            return;
        }
    };

    let (num_regs_i32, num_regs_f32) = match (
        matches.value_of("iregs").unwrap_or("0").parse::<usize>(),
        matches.value_of("fregs").unwrap_or("0").parse::<usize>(),
    ) {
        (Ok(num_i32), Ok(num_f32)) => (num_i32, num_f32),
        _other => {
            println!("invalid iregs/fregs values: {}", matches.usage());
            return;
        }
    };

    let reg_universe = make_universe(num_regs_i32, num_regs_f32);

    if !quiet {
        func.print("before allocation", &None);
    }

    // Just so we can run it later.  Not needed for actual allocation.
    let original_func = func.clone();
    let stackmap_request = func.get_stackmap_request();

    let bump = Bump::new();
    let alloc = Alloc(&bump);
    let result = match allocate_registers_with_opts(
        &mut func,
        &reg_universe,
        stackmap_request.as_ref(),
        opts.clone(),
        &alloc,
    ) {
        Err(e) => {
            println!("allocation failed: {}", e);
            return;
        }
        Ok(r) => r,
    };

    let num_spill_slots = result.num_spill_slots;

    // Update the function itself. This bridges the gap from the generic
    // interface to our specific test ISA.
    let mb_block_anns = result.block_annotations.clone();
    func.update_from_alloc(result);

    if !quiet {
        func.print("after allocation", &mb_block_anns);
    }

    let before_regalloc_result = run_func(
        &original_func,
        "Before allocation",
        &reg_universe,
        RunStage::BeforeRegalloc,
    );

    let after_regalloc_result = run_func(
        &func,
        "After allocation",
        &reg_universe,
        RunStage::AfterRegalloc,
    );

    println!("");
    println!("result before: {:#?}", before_regalloc_result);
    println!("result after: {:#?}", after_regalloc_result);
    println!("number of generated spill slots: {}", num_spill_slots);
    println!("");

    check_results(&before_regalloc_result, &after_regalloc_result);

    if matches.is_present("run-snapshot") {
        println!("Re-running as snapshotted test case...");
        let mut snapshot = IRSnapshot::from_function(&original_func, &reg_universe);
        println!("Constructed snapshot, running...");
        let bump = Bump::new();
        let alloc = Alloc(&bump);
        snapshot
            .allocate(opts, &alloc)
            .expect("generic allocation failed!");
        println!("Success!");
    }
}

fn run_snapshot(path: &str, opts: Options, quiet: bool) {
    use std::fs;
    use std::io::{self, Read};
    use std::path::Path;

    let mut snapshots = Vec::new();

    // First, load all the snapshot files, to not alternate between IO and CPU; then run register
    // allocation on each.

    let path = Path::new(path);

    let mut deserialize_one = |path: &Path, quiet: bool, snapshots: &mut Vec<IRSnapshot>| {
        let mut contents = Vec::new();
        let mut file = fs::File::open(path).expect("couldn't read snapshot path");
        file.read_to_end(&mut contents)
            .expect("couldn't read snapshot file");
        if !quiet {
            println!("Deserializing snapshot...");
        }
        snapshots.push(bincode::deserialize(&contents).expect("couldn't deserialize"));
    };

    if path.is_dir() {
        fn visit_dirs<F: FnMut(&Path, bool, &mut Vec<IRSnapshot>)>(
            path: &Path,
            quiet: bool,
            snapshots: &mut Vec<IRSnapshot>,
            deserialize_one: &mut F,
        ) -> io::Result<()> {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    visit_dirs(&path, quiet, snapshots, deserialize_one)?;
                } else {
                    deserialize_one(&path, quiet, snapshots);
                }
            }
            Ok(())
        };

        visit_dirs(path, quiet, &mut snapshots, &mut deserialize_one)
            .expect("couldn't read at least one directory entry");
    } else {
        deserialize_one(path, quiet, &mut snapshots);
    }

    for (i, mut snapshot) in snapshots.into_iter().enumerate() {
        if !quiet {
            println!("Running regalloc on snapshot {}...", i);
        }
        let bump = Bump::new();
        let alloc = Alloc(&bump);
        match snapshot.allocate(opts.clone(), &alloc) {
            Ok(result) => {
                if !quiet {
                    println!("allocation of snapshotted IR {} worked!", i);
                    println!("num insts: {}", result.insns.len());
                    println!("num spill slots: {}", result.num_spill_slots);
                }
            }
            Err(err) => {
                panic!("allocation failed! {}", err);
            }
        }; // drop the RegAallocResult before the `bump` arena
    }
}

#[cfg(test)]
mod test_utils {
    use regalloc::{allocate_registers, AlgorithmWithDefaults, RegAllocError};

    use super::*;

    pub fn check_bt(func_name: &str, num_gpr: usize, num_fpu: usize) {
        check_bt_internal(func_name, num_gpr, num_fpu, /* use_checker = */ false);
    }

    pub fn _check_bt_checked(func_name: &str, num_gpr: usize, num_fpu: usize) {
        check_bt_internal(func_name, num_gpr, num_fpu, /* use_checker = */ true);
    }

    fn check_bt_internal(func_name: &str, num_gpr: usize, num_fpu: usize, run_checker: bool) {
        let _ = pretty_env_logger::try_init();
        let mut func = test_cases::find_func(func_name).unwrap();
        let reg_universe = make_universe(num_gpr, num_fpu);

        let opts = Options {
            run_checker,
            algorithm: Algorithm::Backtracking(BacktrackingOptions {
                request_block_annotations: false,
            }),
        };

        let bump = Bump::new();
        let alloc = Alloc(&bump);

        let mut encoded = IRSnapshot::from_function(&func, &reg_universe);
        encoded
            .allocate(opts.clone(), &alloc)
            .expect("generic allocator failed!");

        let before_regalloc_result = run_func(
            &func,
            "Before allocation",
            &reg_universe,
            RunStage::BeforeRegalloc,
        );
        let sri = func.get_stackmap_request();
        let bump = Bump::new();
        let alloc = Alloc(&bump);
        let result =
            allocate_registers_with_opts(&mut func, &reg_universe, sri.as_ref(), opts, &alloc)
                .unwrap_or_else(|err| {
                    panic!("allocation failed: {}", err);
                });

        func.update_from_alloc(result);
        let after_regalloc_result = run_func(
            &func,
            "After allocation",
            &reg_universe,
            RunStage::AfterRegalloc,
        );
        check_results(&before_regalloc_result, &after_regalloc_result);
    }

    // Note: num_gpr/num_fpu: must include the scratch register.
    pub fn run_lsra(func_name: &str, num_gpr: usize, num_fpu: usize) -> Result<(), RegAllocError> {
        let _ = pretty_env_logger::try_init();
        let mut func = test_cases::find_func(func_name).unwrap();
        let reg_universe = make_universe(num_gpr, num_fpu);
        let bump = Bump::new();
        let alloc = Alloc(&bump);
        allocate_registers(
            &mut func,
            &reg_universe,
            None,
            AlgorithmWithDefaults::LinearScan,
            &alloc,
        )
        .map(|_| ())
    }

    // Note: num_gpr/num_fpu: must include the scratch register.
    pub fn check_lsra(func_name: &str, num_gpr: usize, num_fpu: usize) {
        let _ = pretty_env_logger::try_init();
        let mut func = test_cases::find_func(func_name).unwrap();
        let reg_universe = make_universe(num_gpr, num_fpu);
        let before_regalloc_result = run_func(
            &func,
            "Before allocation",
            &reg_universe,
            RunStage::BeforeRegalloc,
        );
        func.print("BEFORE", &None);

        let opts = Options {
            run_checker: true,
            algorithm: Algorithm::LinearScan(Default::default()),
        };

        let bump = Bump::new();
        let alloc = Alloc(&bump);

        let mut encoded = IRSnapshot::from_function(&func, &reg_universe);
        encoded
            .allocate(opts.clone(), &alloc)
            .expect("generic allocator failed!");

        let sri = func.get_stackmap_request();
        let result =
            allocate_registers_with_opts(&mut func, &reg_universe, sri.as_ref(), opts, &alloc)
                .unwrap_or_else(|err| {
                    panic!("allocation failed: {}", err);
                });
        func.update_from_alloc(result);
        func.print("AFTER", &None);

        let after_regalloc_result = run_func(
            &func,
            "After allocation",
            &reg_universe,
            RunStage::AfterRegalloc,
        );
        check_results(&before_regalloc_result, &after_regalloc_result);
    }

    // Note: num_gpr/num_fpu: must include the scratch register.
    pub fn loop_lsra(func_name: &str, mut num_gpr: usize) {
        let _ = pretty_env_logger::try_init();
        let func = test_cases::find_func(func_name).unwrap();

        // For the interpreter run, give many real registers.
        let reg_universe = make_universe(32, 32);
        let before_regalloc_result = run_func(
            &func,
            "Before allocation",
            &reg_universe,
            RunStage::BeforeRegalloc,
        );
        func.print("BEFORE", &None);
        let sri = func.get_stackmap_request();

        let opts = Options {
            run_checker: false,
            algorithm: Algorithm::LinearScan(Default::default()),
        };
        loop {
            println!("for num_gpr = {}", num_gpr);

            let mut func = func.clone();
            let reg_universe = make_universe(num_gpr, 0);

            let bump = Bump::new();
            let alloc = Alloc(&bump);

            let mut encoded = IRSnapshot::from_function(&func, &reg_universe);
            encoded
                .allocate(opts.clone(), &alloc)
                .expect("generic allocator failed!");

            let result = allocate_registers_with_opts(
                &mut func,
                &reg_universe,
                sri.as_ref(),
                opts.clone(),
                &alloc,
            )
            .expect("regalloc failure");

            func.update_from_alloc(result);
            func.print("AFTER", &None);

            let after_regalloc_result = run_func(
                &func,
                "After allocation",
                &reg_universe,
                RunStage::AfterRegalloc,
            );

            check_results(&before_regalloc_result, &after_regalloc_result);

            if let Ok(results) = after_regalloc_result {
                if results.num_reloads == 0 {
                    break;
                }
            }
            num_gpr += 1;
        }
    }
}

// At some point we'll want to repeat all these tests with the number of
// registers iterating down to 3, so as to stress the spilling machinery as
// much as we can.

// Badness requires 0 registers, so any combination should just work fine.
#[test]
fn bt_badness() {
    test_utils::check_bt("badness", 1, 0);
}
#[test]
fn lsra_badness() {
    test_utils::check_lsra("badness", 2, 0);
}

// straight_line requires one register.
#[test]
fn bt_straight_line() {
    test_utils::check_bt("straight_line", 1, 0);
}
#[test]
fn lsra_straight_line() {
    test_utils::check_lsra("straight_line", 2, 0);
}

// fill_then_sum requires 3 registers (it mentions r2 explicitly).
#[test]
fn bt_fill_then_sum() {
    test_utils::check_bt("fill_then_sum", 8, 8);
}
#[test]
fn lsra_fill_then_sum() {
    assert!(test_utils::run_lsra("fill_then_sum", 1, 0).is_err());
    assert!(test_utils::run_lsra("fill_then_sum", 2, 0).is_err());
    // We can't test 3 here, because there's a panic in the code, since r2 is the
    // scratch register by definition. Not so bad.
    test_utils::loop_lsra("fill_then_sum", 4);
}

// ssort requires at least 2 registers.
#[test]
fn bt_ssort() {
    test_utils::check_bt("ssort", 8, 8);
}
#[test]
fn btc_ssort() {
    test_utils::check_bt("ssort", 8, 8);
    //TODO reenable once #47 is fixed.
    //test_utils::check_bt_checked("ssort", 8, 8);
}
#[test]
fn lsra_ssort_3() {
    test_utils::check_lsra("ssort", 3, 0);
}
#[test]
fn lsra_ssort_4() {
    test_utils::check_lsra("ssort", 4, 0);
}
#[test]
fn lsra_ssort_5() {
    test_utils::check_lsra("ssort", 5, 0);
}
#[test]
fn lsra_ssort_6() {
    test_utils::check_lsra("ssort", 6, 0);
}
#[test]
fn lsra_ssort_7() {
    test_utils::check_lsra("ssort", 7, 0);
}
#[test]
fn lsra_ssort_8() {
    test_utils::check_lsra("ssort", 8, 0);
}

// Requires 2 registers.
#[test]
fn lsra_ssort() {
    test_utils::loop_lsra("ssort2", 3);
}

// 3_loops requires at least 2 registers.
#[test]
fn bt_3_loops() {
    test_utils::check_bt("3_loops", 8, 8);
}
#[test]
fn lsra_3_loops() {
    assert!(test_utils::run_lsra("3_loops", 1, 0).is_err());
    assert!(test_utils::run_lsra("3_loops", 2, 0).is_err());
    test_utils::loop_lsra("3_loops", 3);
}

// stmts requires at least 2 registers.
#[test]
fn bt_stmts() {
    test_utils::check_bt("stmts", 8, 8);
}
#[test]
fn lsra_stmts() {
    assert!(test_utils::run_lsra("stmts", 1, 0).is_err());
    assert!(test_utils::run_lsra("stmts", 2, 0).is_err());
    test_utils::loop_lsra("stmts", 3);
}

// needs_splitting requires at least 2 registers.
#[test]
fn bt_needs_splitting() {
    test_utils::check_bt("needs_splitting", 8, 8);
}
#[test]
fn lsra_needs_splitting() {
    assert!(test_utils::run_lsra("needs_splitting", 1, 0).is_err());
    assert!(test_utils::run_lsra("needs_splitting", 2, 0).is_err());
    test_utils::loop_lsra("needs_splitting", 3);
}

// needs_splitting2 requires at least 2 registers.
#[test]
fn bt_needs_splitting2() {
    test_utils::check_bt("needs_splitting2", 8, 8);
}
#[test]
fn lsra_needs_splitting2() {
    assert!(test_utils::run_lsra("needs_splitting2", 1, 0).is_err());
    assert!(test_utils::run_lsra("needs_splitting2", 2, 0).is_err());
    test_utils::loop_lsra("needs_splitting2", 3);
}

// qsort requires at least 3 registers.
// The following test are put in several functions because this takes a lot of
// time to interpret, and putting these in a single function would slow down the
// testing pipeline a lot.
#[test]
fn bt_qsort() {
    test_utils::check_bt("qsort", 8, 8);
}

#[test]
fn btc_qsort() {
    test_utils::check_bt("qsort", 8, 8);
    //TODO reenable once #47 is fixed.
    //test_utils::check_bt_checked("qsort", 8, 8);
}

#[test]
fn lsra_qsort_cant() {
    assert!(test_utils::run_lsra("qsort", 1, 0).is_err());
    assert!(test_utils::run_lsra("qsort", 2, 0).is_err());
    assert!(test_utils::run_lsra("qsort", 3, 0).is_err());
}
#[test]
fn lsra_qsort_4() {
    test_utils::check_lsra("qsort", 4, 0);
}
#[test]
fn lsra_qsort_5() {
    test_utils::check_lsra("qsort", 5, 0);
}
#[test]
fn lsra_qsort_6() {
    test_utils::check_lsra("qsort", 6, 0);
}

#[test]
fn lsra_qsort_7() {
    test_utils::check_lsra("qsort", 7, 0);
}
#[test]
fn lsra_qsort_8() {
    test_utils::check_lsra("qsort", 8, 0);
}
#[test]
fn lsra_qsort_9() {
    test_utils::check_lsra("qsort", 9, 0);
}
#[test]
fn lsra_qsort_10() {
    test_utils::check_lsra("qsort", 10, 0);
}
#[test]
fn lsra_qsort_11() {
    test_utils::check_lsra("qsort", 11, 0);
}
#[test]
fn lsra_qsort_12() {
    test_utils::check_lsra("qsort", 12, 0);
}
#[test]
fn lsra_qsort_13() {
    test_utils::check_lsra("qsort", 13, 0);
}
#[test]
fn lsra_qsort_14() {
    test_utils::check_lsra("qsort", 14, 0);
}
#[test]
fn lsra_qsort_15() {
    test_utils::check_lsra("qsort", 15, 0);
}
#[test]
fn lsra_qsort_16() {
    test_utils::check_lsra("qsort", 16, 0);
}
#[test]
fn lsra_qsort_17() {
    test_utils::check_lsra("qsort", 17, 0);
}
#[test]
fn lsra_qsort_18() {
    test_utils::check_lsra("qsort", 18, 0);
}

#[test]
fn lsra_critical_edge_invariant() {
    assert!(matches!(
        test_utils::run_lsra("lsra_invariant_incorrect", 5, 0),
        Err(regalloc::RegAllocError::Analysis(
            regalloc::AnalysisError::LsraCriticalEdge { .. }
        ))
    ));
}

#[test]
fn lsra_fuzz_block_start_end() {
    test_utils::check_lsra("lsra_fuzz_block_start_end", 5, 0);
}

// Requires at least 3 registers (r2 is mentioned explicitly).
#[test]
fn bt_fill_then_sum_2a() {
    test_utils::check_bt("fill_then_sum_2a", 8, 8);
}
#[test]
fn lsra_fill_then_sum_2a() {
    assert!(test_utils::run_lsra("fill_then_sum_2a", 1, 0).is_err());
    assert!(test_utils::run_lsra("fill_then_sum_2a", 2, 0).is_err());
    // See comment in lsra_fill_then_sum for 3 registers.
    test_utils::loop_lsra("fill_then_sum_2a", 4);
}

// Requires at least 2 registers.
#[test]
fn bt_ssort_2a() {
    test_utils::check_bt("ssort_2a", 8, 8);
}
#[test]
fn lsra_2a_ssort() {
    assert!(test_utils::run_lsra("ssort_2a", 1, 0).is_err());
    assert!(test_utils::run_lsra("ssort_2a", 2, 0).is_err());
    test_utils::loop_lsra("ssort_2a", 3);
}

// Requires 1 GPR and 2 FPUs at least.
#[test]
fn bt_fp1() {
    test_utils::check_bt("fp1", 8, 8);
}
#[test]
fn lsra_fp1() {
    assert!(test_utils::run_lsra("fp1", 2, 1).is_err());
    assert!(test_utils::run_lsra("fp1", 1, 2).is_err());
    assert!(test_utils::run_lsra("fp1", 2, 2).is_err());
    test_utils::check_lsra("fp1", 2, 3);
}

// Requires 2 GPRs and 2 FPUs at least.
#[test]
fn bt_fp2() {
    test_utils::check_bt("fp2", 8, 8);
}
#[test]
fn lsra_fp2() {
    for i in 3..8 {
        for j in 3..8 {
            test_utils::check_lsra("fp2", i, j);
        }
    }
}

// Requires at least 1 GPR.
#[test]
fn lsra_simple_spill() {
    test_utils::loop_lsra("simple_spill", 2);
}

// Requires at least 2 GPRs.
#[test]
fn lsra_simple_loop() {
    test_utils::loop_lsra("simple_loop", 3);
}

// Requires at least 2 GPRs.
#[test]
fn lsra_stmt_loop() {
    test_utils::loop_lsra("stmt_loop", 3)
}

#[test]
fn lsra_stmt_repeat() {
    test_utils::loop_lsra("stmt_repeat", 3)
}

#[test]
fn any_use_modified() {
    test_utils::check_bt("use_mod", 1, 0);
}

#[test]
fn lsra_blocked_fixed() {
    test_utils::check_lsra("blocked_fixed", 5, 2);
}
#[test]
fn lsra_multi_split() {
    test_utils::check_lsra("lsra_multi_split", 5, 0);
}
#[test]
fn lsra_double_succ() {
    test_utils::check_lsra("lsra_double_succ", 5, 2);
}
#[test]
fn lsra_inblock_fixup_pos() {
    test_utils::check_lsra("lsra_inblock_fixup_pos", 5, 2);
}
#[test]
fn lsra_parallel_reloads() {
    test_utils::check_lsra("lsra_parallel_reloads", 5, 2);
}
#[test]
fn lsra_flush_block_fixups() {
    test_utils::check_lsra("lsra_flush_block_fixups", 5, 2);
}
#[test]
fn lsra_split_unused() {
    test_utils::check_lsra("lsra_split_unused", 5, 2);
}
#[test]
fn lsra_split_unused_2() {
    test_utils::check_lsra("lsra_split_unused_2", 5, 2);
}
#[test]
fn lsra_split_positions() {
    test_utils::check_lsra("lsra_split_positions", 5, 2);
}

#[test]
fn bt_analysis_fuzz1() {
    test_utils::check_bt("fuzz1", 3, 3);
}
#[test]
fn bt_analysis_fuzz2() {
    test_utils::check_bt("fuzz2", 3, 3);
}

#[test]
fn lsra_analysis_fuzz1() {
    test_utils::check_lsra("fuzz1", 3, 3);
}
#[test]
fn lsra_analysis_fuzz2() {
    test_utils::check_lsra("fuzz2", 3, 3);
}

#[test]
fn lsra_sort_fixed() {
    test_utils::check_lsra("lsra_sort_fixed", 5, 5);
}

#[test]
fn bt_fuzz_stackmap() {
    test_utils::check_bt("stackmap", 5, 5);
    test_utils::check_bt("fuzz_stackmap", 5, 5);
    test_utils::check_bt("fuzz_stackmap2", 5, 5);
    test_utils::check_bt("fuzz_stackmap3", 5, 5);
}

#[test]
fn lsra_fuzz_stackmap() {
    test_utils::check_lsra("stackmap", 5, 5);
    test_utils::check_lsra("fuzz_stackmap", 5, 5);
    test_utils::check_lsra("fuzz_stackmap2", 5, 5);
    test_utils::check_lsra("fuzz_stackmap3", 5, 5);
}
