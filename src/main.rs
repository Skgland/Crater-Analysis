use std::{borrow::Cow, collections::BTreeMap, env::args, path::Path, time::Duration};

use futures::StreamExt as _;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
enum AnalysisError {
    Reqwest(#[from] reqwest::Error),
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
}

#[tokio::main]
async fn main() -> Result<(), AnalysisError> {
    let logger = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .build();

    let multi = MultiProgress::new();
    LogWrapper::new(multi.clone(), logger).try_init().unwrap();

    let reports = futures::stream::iter(args().skip(1))
        .map(|experiment| {
            let multi = multi.clone();
            async move { run_analysis(&experiment, multi).await }
        })
        .buffered(5)
        .collect::<Vec<_>>()
        .await;

    for report in reports {
        println!("");
        report?.print_report();
    }

    Ok(())
}

async fn run_analysis(
    experiment: &str,
    multi: MultiProgress,
) -> Result<AnalysisReport, AnalysisError> {
    if let Err(err) = std::fs::create_dir_all(format!("./results/{experiment}/logs")) {
        log::warn!("Failed to ensure cache dir exists: {err}");
    }

    let report_ps = multi.add(
        ProgressBar::new_spinner().with_message(format!("Getting Crater Report for {experiment}")),
    );
    report_ps.enable_steady_tick(Duration::from_millis(100));
    let report = get_report(experiment).await?;
    report_ps.set_message(format!("Processing Crater Report for {experiment}"));

    let mut other = Vec::new();

    let mut regressed_count = 0;

    let unknown_runs = report
        .crates
        .iter()
        .filter(|krate| krate.res == "regressed")
        .inspect(|_| {
            regressed_count += 1;
        })
        .flat_map(|krate| krate.runs.iter().flatten().map(|run| (&krate.name, run)))
        .filter(|(_, run)| run.res == "build-fail:unknown")
        .collect::<Vec<_>>();

    let unknown_build_fail_results = unknown_runs.len();
    let run_pb = multi.add(
        ProgressBar::new(unknown_build_fail_results as u64)
            .with_message(format!("Processing logs for {experiment}")),
    );
    run_pb.set_style(
        ProgressStyle::with_template("{msg} {wide_bar} {human_pos}/{human_len}").unwrap(),
    );

    let mut stream = futures::stream::iter(unknown_runs)
        .map(|(krate_name, run)| {
            let experiment = &experiment;
            async move {
                let log = get_log(experiment, &run.log).await;
                (krate_name, run, log)
            }
        })
        .buffer_unordered(20);

    let targets = [
        (
            "docker",
            [
                "[INFO] [stderr] Error response from daemon:",
                ": no such file or directory",
            ]
            .as_slice(),
        ),
        (
            "docker",
            &[
                "[INFO] [stderr] Error response from daemon:",
                ": file exists",
            ],
        ),
        ("compile_error!", &["compile_error!"]),
        (
            "missing-env-var",
            &["note: this error originates in the macro `env`"],
        ),
        (
            "delimiter missmatch",
            &["error: mismatched closing delimiter:"],
        ),
        ("no-space", &["no space left on device"]),
        (
            "linker-bus-error",
            &["collect2: fatal error: ld terminated with signal 7 [Bus error]"],
        ),
        ("useless-conversion", &["error: this conversion is useless"]),
        (
            "build-script",
            &["[INFO] [stderr] error: failed to run custom build command for"],
        ),
        ("download", &["[INFO] [stderr] error: failed to download"]),
        (
            "linker-undefined-symbol",
            &["rust-lld: error: undefined symbol:"],
        ),
        (
            "linker-missing-library",
            &["rust-lld: error: unable to find library"],
        ),
        (
            "linker-write-output",
            &[
                "rust-lld: error: failed to write output",
                "No such file or directory",
            ],
        ),
        (
            "include_str-missing-file",
            &["note: this error originates in the macro `include_str`"],
        ),
        (
            "include_bytes-missing-file",
            &["note: this error originates in the macro `include_bytes`"],
        ),
        ("ice", &["error: internal compiler error:"]),
    ];

    let mut findings = BTreeMap::<Cow<'static, str>, usize>::new();

    let error_regex = regex::RegexBuilder::new(r#"^\[INFO\] \[stdout\] error\[(E\d+)\]:"#)
        .multi_line(true)
        .build()
        .unwrap();

    while let Some((krate_name, run, log)) = stream.next().await {
        match log {
            Ok(log) => {
                let mut has_reason = false;

                for line in log.lines() {
                    for (target_name, target) in targets {
                        if target.iter().all(|pat| line.contains(pat)) {
                            *findings.entry(target_name.into()).or_default() += 1;
                            has_reason = true;
                        }
                    }
                }

                for needle in error_regex.captures_iter(&log) {
                    if let Some(capture) = needle.get(1) {
                        *findings
                            .entry(capture.as_str().to_string().into())
                            .or_default() += 1;
                        has_reason = true;
                    }
                }

                if !has_reason {
                    other.push((krate_name, &run.log));
                }
            }

            Err(err) => {
                log::warn!("Failed to get log '{}': {err}", run.log);
            }
        }

        run_pb.inc(1);
    }

    run_pb.finish_with_message(format!("Processed logs for {experiment}"));
    report_ps.finish_with_message(format!("Processed Crated Report for {experiment}"));

    Ok(AnalysisReport {
        experiment: experiment.to_string(),
        regressed_count: regressed_count,
        unknown_build_fail_results,
        findings,
        other: other
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
    })
}

struct AnalysisReport {
    experiment: String,
    regressed_count: usize,
    unknown_build_fail_results: usize,
    findings: BTreeMap<Cow<'static, str>, usize>,
    other: Vec<(String, String)>,
}

impl AnalysisReport {
    pub fn print_report(&self) {
        println!("Report for Crater Experiment {}", self.experiment);
        println!("Regressed: {}", self.regressed_count);
        println!("build failed(unknown): {}", self.unknown_build_fail_results);
        println!("----------------------------------");
        println!("Results:");

        for (name, &count) in &self.findings {
            println!("{name}: {count}")
        }

        let sum: usize = self.findings.values().sum();
        println!("----------------------------------");
        println!("sum: {sum}");
        println!("others: {}", self.other.len());
        println!("----------------------------------");
        println!("{:#?}", self.other);
    }
}

async fn get_log(experiment: &str, log: &str) -> Result<String, AnalysisError> {
    let log_folder = format!("./results/{experiment}/logs/{log}");
    if let Err(err) = std::fs::create_dir_all(&log_folder) {
        log::warn!("Failed to create cache folder: {err}");
    }
    let log_path = format!("{log_folder}/log.txt");
    let log_url = format!("https://crater-reports.s3.amazonaws.com/{experiment}/{log}/log.txt");

    get_or_download_file(log_path.as_ref(), &log_url).await
}

#[derive(serde::Deserialize)]
struct Results {
    crates: Vec<CrateResult>,
}

#[derive(serde::Deserialize, Debug)]
struct CrateResult {
    name: String,
    #[allow(dead_code)]
    url: Option<String>,
    res: String,
    runs: Vec<Option<RunResult>>,
}

#[derive(serde::Deserialize, Debug)]
struct RunResult {
    res: String,
    log: String,
}

async fn get_report(expriment: &str) -> Result<Results, AnalysisError> {
    let result_json_path = format!("./results/{expriment}/results.json");
    let result_json_url =
        format!("https://crater-reports.s3.amazonaws.com/{expriment}/results.json");
    let results = get_or_download_file(result_json_path.as_ref(), &result_json_url).await?;
    Ok(serde_json::from_str(&results)?)
}

async fn get_or_download_file(
    cache_path: &Path,
    download_url: &str,
) -> Result<String, AnalysisError> {
    let resuls = match tokio::fs::read_to_string(cache_path).await {
        Ok(content) => {
            log::debug!("Using cached file");
            content
        }
        Err(err) => {
            let entry = if let Some(parent) = cache_path.parent() {
                if let Some(name) = parent.file_name() {
                    name.to_string_lossy().into_owned()
                } else {
                    "parent-has-no-name".to_string()
                }
            } else {
                "no-parent".to_string()
            };

            log::debug!(
                "Failed to access cached resuls for {entry}, falling back to downloading: {err}"
            );
            let response = reqwest::get(download_url).await?;
            let content = response.text().await?;
            if let Err(err) = tokio::fs::write(cache_path, &content).await {
                log::warn!("Failed to cache result to {cache_path:?}: {err}");
            }
            content
        }
    };
    Ok(resuls)
}
