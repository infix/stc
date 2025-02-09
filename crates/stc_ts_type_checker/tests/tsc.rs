#![recursion_limit = "256"]
#![feature(box_syntax)]
#![feature(box_patterns)]
#![feature(test)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::manual_strip)]

extern crate test;

#[path = "common/mod.rs"]
mod common;

use std::{
    env, fs,
    fs::{read_to_string, File},
    mem,
    panic::{catch_unwind, resume_unwind},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::Deserialize;
use stc_ts_env::Env;
use stc_ts_file_analyzer::env::EnvFactory;
use stc_ts_module_loader::resolvers::node::NodeResolver;
use stc_ts_testing::conformance::{parse_conformance_test, TestSpec};
use stc_ts_type_checker::{loader::ModuleLoader, Checker};
use swc_common::{
    errors::{DiagnosticBuilder, DiagnosticId},
    input::SourceFileInput,
    FileName, SourceMap, Span,
};
use swc_ecma_parser::{Parser, Syntax, TsConfig};
use swc_ecma_visit::Fold;
use test::test_main;
use testing::{StdErr, Tester};

use self::common::load_fixtures;

struct RecordOnPanic {
    stats_file_name: PathBuf,
    stats: Stats,
}

impl Drop for RecordOnPanic {
    fn drop(&mut self) {
        let stats = Stats {
            panic: 1,
            ..self.stats.clone()
        };
        print_per_test_stat(&self.stats_file_name, &stats);
        record_stat(stats);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, PartialOrd, Ord)]
struct RefError {
    pub line: usize,
    pub column: usize,
    pub code: String,
}

#[derive(Debug, Default, Clone)]
struct Stats {
    required_error: usize,
    /// Correct error count.
    matched_error: usize,
    /// False-positive error count.
    extra_error: usize,

    /// Tests failed with panic
    panic: usize,
}

fn is_all_test_enabled() -> bool {
    env::var("TEST").map(|s| s.is_empty()).unwrap_or(false)
}

fn print_matched_errors() -> bool {
    !env::var("DO_NOT_PRINT_MATCHED").map(|s| s == "1").unwrap_or(false)
}

fn record_time(line_count: usize, time_of_check: Duration, full_time: Duration) {
    static TOTAL_CHECK: Lazy<Mutex<Duration>> = Lazy::new(Default::default);
    static TOTAL_FULL: Lazy<Mutex<Duration>> = Lazy::new(Default::default);
    static LINES: Lazy<Mutex<usize>> = Lazy::new(Default::default);

    if cfg!(debug_assertions) {
        return;
    }

    let time_of_check = {
        let mut guard = TOTAL_CHECK.lock();
        *guard += time_of_check;
        *guard
    };
    let full_time = {
        let mut guard = TOTAL_FULL.lock();
        *guard += full_time;
        *guard
    };

    let line_count = {
        let mut guard = LINES.lock();
        *guard += line_count;
        *guard
    };

    let content = format!(
        "{:#?}",
        Timings {
            lines: line_count,
            check_time: time_of_check,
            full_time
        }
    );

    #[allow(dead_code)]
    #[derive(Debug)]
    struct Timings {
        lines: usize,
        check_time: Duration,
        full_time: Duration,
    }

    // If we are testing everything, update stats file.
    if is_all_test_enabled() {
        fs::write("tests/tsc.timings.rust-debug", &content).unwrap();
    }
}

/// Add stats and return total stats.
fn record_stat(stats: Stats) -> Stats {
    static STATS: Lazy<Mutex<Stats>> = Lazy::new(Default::default);

    if !cfg!(debug_assertions) {
        return stats;
    }

    let mut guard = STATS.lock();
    guard.required_error += stats.required_error;
    guard.matched_error += stats.matched_error;
    guard.extra_error += stats.extra_error;
    guard.panic += stats.panic;

    let stats = (*guard).clone();

    drop(guard);

    let content = format!("{:#?}", stats);

    // If we are testing everything, update stats file.
    if is_all_test_enabled() {
        fs::write("tests/tsc-stats.rust-debug", &content).unwrap();
    }

    stats
}

/// Returns **path**s (separated by `/`) of tests.
fn load_list(name: &str) -> Vec<String> {
    let content = read_to_string(name).unwrap();

    content
        .lines()
        .map(|v| v.replace("::", "/"))
        .filter(|v| !v.trim().is_empty())
        .collect()
}

fn is_ignored(path: &Path) -> bool {
    static IGNORED: Lazy<Vec<String>> = Lazy::new(|| load_list("tests/tsc.ignored.txt"));

    static PASS: Lazy<Vec<String>> = Lazy::new(|| {
        let mut v = load_list("tests/conformance.pass.txt");
        v.extend(load_list("tests/compiler.pass.txt"));
        if env::var("STC_IGNORE_WIP").unwrap_or_default() != "1" {
            v.extend(load_list("tests/tsc.wip.txt"));
        }
        v
    });

    if IGNORED.iter().any(|line| path.to_string_lossy().contains(line)) {
        return true;
    }

    if let Ok(test) = env::var("TEST") {
        return !path.to_string_lossy().contains(&test);
    }

    !PASS.iter().any(|line| path.to_string_lossy().contains(line))
}

#[test]
fn conformance() {
    let args: Vec<_> = env::args().collect();
    let tests = load_fixtures("conformance", create_test);
    // tests.extend(load_fixtures("compiler", create_test));
    test_main(&args, tests, Default::default());
}

fn is_parser_test(errors: &[RefError]) -> bool {
    for err in errors {
        if err.code.starts_with("TS1") && err.code.len() == 6 {
            return true;
        }

        // These are actually parser test.
        if let "TS2369" = &*err.code {
            return true;
        }
    }

    false
}

fn create_test(path: PathBuf) -> Option<Box<dyn FnOnce() + Send + Sync>> {
    if is_ignored(&path) {
        return None;
    }

    let specs = catch_unwind(|| parse_conformance_test(&path)).ok()?;
    let use_target = specs.len() > 1;

    if use_target {
        for spec in specs.iter() {
            if is_parser_test(&load_expected_errors(&path, Some(spec)).1) {
                return None;
            }
        }
    } else if is_parser_test(&load_expected_errors(&path, None).1) {
        return None;
    }

    let str_name = path.display().to_string();

    // If parser returns error, ignore it for now.

    let cm = SourceMap::default();
    cm.new_source_file(FileName::Anon, "".into());

    let fm = cm.load_file(&path).unwrap();

    // Ignore parser error tests
    catch_unwind(|| {
        let mut parser = Parser::new(
            Syntax::Typescript(TsConfig {
                tsx: str_name.contains("tsx"),
                ..Default::default()
            }),
            SourceFileInput::from(&*fm),
            None,
        );
        parser.parse_module().ok()
    })
    .ok()??;

    Some(box move || {
        let mut last = None;
        for spec in specs {
            let res = catch_unwind(|| {
                do_test(&path, spec, use_target).unwrap();
            });
            if let Err(err) = res {
                last = Some(err);
            }
        }

        if let Some(last) = last {
            resume_unwind(last);
        }
    })
}

/// If `spec` is [Some], it's use to construct filename.
///
/// Returns `(file_suffix, errors)`
fn load_expected_errors(ts_file: &Path, spec: Option<&TestSpec>) -> (String, Vec<RefError>) {
    let errors_file = match spec {
        Some(v) => ts_file.with_file_name(format!(
            "{}(target={}).errors.json",
            ts_file.file_stem().unwrap().to_string_lossy(),
            v.raw_target
        )),
        None => ts_file.with_extension("errors.json"),
    };

    let errors = if !errors_file.exists() {
        println!("errors file does not exists: {}", errors_file.display());
        vec![]
    } else {
        let mut errors: Vec<RefError> = serde_json::from_reader(File::open(&errors_file).expect("failed to open errors file"))
            .context("failed to parse errors.txt.json")
            .unwrap();

        for err in &mut errors {
            let orig_code = err.code.replace("TS", "").parse().expect("failed to parse error code");
            let code = stc_ts_errors::ErrorKind::normalize_error_code(orig_code);

            if orig_code != code {
                err.code = format!("TS{}", code);
            }
        }

        // TODO(kdy1): Match column and message

        errors
    };

    (
        errors_file.file_name().unwrap().to_string_lossy().replace(".errors.json", ""),
        errors,
    )
}

fn do_test(file_name: &Path, spec: TestSpec, use_target: bool) -> Result<(), StdErr> {
    let (file_stem, mut expected_errors) = load_expected_errors(file_name, if use_target { Some(&spec) } else { None });
    expected_errors.sort();

    let stats_file_name = file_name.with_file_name(format!("{}.stats.rust-debug", file_stem));

    let TestSpec {
        err_shift_n,
        libs,
        rule,
        target,
        module_config,
        raw_target: _,
        ..
    } = spec;

    let stat_guard = RecordOnPanic {
        stats_file_name: stats_file_name.clone(),
        stats: Stats {
            required_error: expected_errors.len(),
            ..Default::default()
        },
    };

    {
        let src = fs::read_to_string(file_name).unwrap();
        // Postpone multi-file tests.
        if src.to_lowercase().contains("@filename") || src.contains("<reference path") {
            if is_all_test_enabled() {
                record_stat(Stats {
                    required_error: expected_errors.len(),
                    ..Default::default()
                });
            }

            return Ok(());
        }
    }
    let mut time_of_check = Duration::new(0, 0);
    let mut full_time = Duration::new(0, 0);

    let mut stats = Stats::default();
    dbg!(&libs);
    for err in &mut expected_errors {
        // This error use special span.
        if err.line == 0 {
            continue;
        }
        // Typescript conformance test remove lines starting with @-directives.
        err.line += err_shift_n;
    }

    let full_ref_errors = expected_errors.clone();
    let full_ref_err_cnt = full_ref_errors.len();

    let tester = Tester::new();
    let diagnostics = tester
        .errors(|cm, handler| {
            let handler = Arc::new(handler);
            let env = Env::simple(rule, target, module_config, &libs);
            let mut checker = Checker::new(
                cm.clone(),
                handler.clone(),
                env.clone(),
                None,
                ModuleLoader::new(cm, env, NodeResolver),
            );

            // Install a logger
            let _guard = testing::init();

            let start = Instant::now();

            checker.check(Arc::new(FileName::Real(file_name.into())));

            let end = Instant::now();

            time_of_check = end - start;

            let errors = ::stc_ts_errors::ErrorKind::flatten(checker.take_errors());

            for e in errors {
                e.emit(&handler);
            }

            let end = Instant::now();

            full_time = end - start;

            if false {
                return Ok(());
            }

            Err(())
        })
        .expect_err("");

    mem::forget(stat_guard);

    if !cfg!(debug_assertions) {
        let line_cnt = {
            let content = fs::read_to_string(file_name).unwrap();

            content.lines().count()
        };
        record_time(line_cnt, time_of_check, full_time);

        // if time > Duration::new(0, 500_000_000) {
        //     let _ = fs::write(file_name.with_extension("timings.txt"),
        // format!("{:?}", time)); }
    }

    let mut extra_errors = diagnostics
        .iter()
        .map(|d| {
            let span = d.span.primary_span().unwrap();
            let cp = tester.cm.lookup_char_pos(span.lo());
            let code = d
                .code
                .clone()
                .expect("conformance testing: All errors should have proper error code");
            let code = match code {
                DiagnosticId::Error(err) => err,
                DiagnosticId::Lint(lint) => {
                    unreachable!("Unexpected lint '{}' found", lint)
                }
            };

            (cp.line, code)
        })
        .collect::<Vec<_>>();
    extra_errors.sort();

    let full_actual_errors = extra_errors.clone();

    for (line, error_code) in full_actual_errors.clone() {
        if let Some(idx) = expected_errors
            .iter()
            .position(|err| (err.line == line || err.line == 0) && err.code == error_code)
        {
            stats.matched_error += 1;

            let is_zero_line = expected_errors[idx].line == 0;
            expected_errors.remove(idx);
            if let Some(idx) = extra_errors
                .iter()
                .position(|(r_line, r_code)| (line == *r_line || is_zero_line) && error_code == *r_code)
            {
                extra_errors.remove(idx);
            }
        }
    }

    //
    //      - All reference errors are matched
    //      - Actual errors does not remain
    let success = expected_errors.is_empty() && extra_errors.is_empty();

    let res: Result<(), _> = tester.print_errors(|_, handler| {
        // If we failed, we only emit errors which has wrong line.

        for (d, line_col) in diagnostics.into_iter().zip(full_actual_errors.clone()) {
            if success || env::var("PRINT_ALL").unwrap_or_default() == "1" || extra_errors.contains(&line_col) {
                DiagnosticBuilder::new_diagnostic(&handler, d).emit();
            }
        }

        Err(())
    });

    let err = match res {
        Ok(_) => StdErr::from(String::from("")),
        Err(err) => err,
    };

    let extra_err_count = extra_errors.len();
    stats.required_error += expected_errors.len();
    stats.extra_error += extra_err_count;

    // Print per-test stats so we can prevent regressions.
    if cfg!(debug_assertions) {
        print_per_test_stat(&stats_file_name, &stats);
    }

    let total_stats = record_stat(stats);

    if cfg!(debug_assertions) {
        println!("[TOTAL_STATS] {:#?}", total_stats);

        if expected_errors.is_empty() {
            println!("[REMOVE_ONLY]{}", file_name.display());
        }
    }

    if extra_errors.len() == expected_errors.len() {
        let expected_lines = expected_errors.iter().map(|v| v.line).collect::<Vec<_>>();
        let extra_lines = extra_errors.iter().map(|(v, _)| *v).collect::<Vec<_>>();

        if expected_lines == extra_lines {
            println!("[ERROR_CODE_ONLY]{}", file_name.display());
        }
    }

    if print_matched_errors() {
        eprintln!(
            "\n============================================================\n{:?}
============================================================\n{} unmatched errors out of {} errors. Got {} extra errors.\nWanted: \
             {:?}\nUnwanted: {:?}\n\nAll required errors: {:?}\nAll actual errors: {:?}",
            err,
            expected_errors.len(),
            full_ref_err_cnt,
            extra_err_count,
            expected_errors,
            extra_errors,
            full_ref_errors,
            full_actual_errors,
        );
    } else {
        eprintln!(
            "\n============================================================\n{:?}
============================================================\n{} unmatched errors out of {} errors. Got {} extra errors.\nWanted: \
             {:?}\nUnwanted: {:?}",
            err,
            expected_errors.len(),
            full_ref_err_cnt,
            extra_err_count,
            expected_errors,
            extra_errors,
        );
    }
    if !success {
        panic!()
    }

    Ok(())
}

fn print_per_test_stat(stats_file_name: &Path, stats: &Stats) {
    if env::var("CI").unwrap_or_default() == "1" {
        let stat_string = fs::read_to_string(stats_file_name).expect("failed to read test stats file");

        assert_eq!(format!("{:#?}", stats), stat_string, "CI=1 so test stats must match");
    } else {
        fs::write(stats_file_name, format!("{:#?}", stats)).expect("failed to write test stats");
    }
}

struct Spanner {
    span: Span,
}

impl Fold for Spanner {
    fn fold_span(&mut self, _: Span) -> Span {
        self.span
    }
}
