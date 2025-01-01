use std::{collections::BTreeMap, env::args, path::Path, time::Duration};

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

    let expriment = args().nth(1).unwrap();

    run_analysis(&expriment, multi).await
}

async fn run_analysis(experiment: &str, multi: MultiProgress) -> Result<(), AnalysisError> {
    if let Err(err) = std::fs::create_dir_all(format!("./results/{experiment}/logs")) {
        log::warn!("Failed to ensure cache dir exists: {err}");
    }

    let report_ps = multi.add(ProgressBar::new_spinner().with_message("Getting Crater Report"));
    report_ps.enable_steady_tick(Duration::from_millis(100));
    let report = get_report(experiment).await?;
    report_ps.finish_and_clear();

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

    let run_pb =
        multi.add(ProgressBar::new(unknown_runs.len() as u64).with_message("Processing logs"));
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
        ("E0277", "error[E0277]:"),
        ("E0308", "error[E0308]: mismatched types"),
        (
            "E0422",
            "error[E0422]: cannot find struct, variant or union type",
        ),
        ("E0463", "error[E0463]: can't find crate for"),
        ("E0557", "error[E0557]: feature has been removed"),
        ("E0635", "error[E0635]: unknown feature"),
        ("E0685", "error[E0658]: use of unstable library feature"),
        ("compile_error!", "compile_error!"),
        (
            "missing-env-var",
            "note: this error originates in the macro `env`",
        ),
        (
            "delimiter missmatch",
            "error: mismatched closing delimiter:",
        ),
        ("no-space", "no space left on device"),
        (
            "linker-bus-error",
            "collect2: fatal error: ld terminated with signal 7 [Bus error]",
        ),
        (
            "linker-undefined-symbol",
            "rust-lld: error: undefined symbol:",
        ),
        (
            "linker-missing-library",
            "rust-lld: error: unable to find library",
        ),
        (
            "include_str-missing-file",
            "note: this error originates in the macro `include_str`",
        ),
        (
            "include_bytes-missing-file",
            "note: this error originates in the macro `include_bytes`",
        ),
        ("ice", "error: internal compiler error:"),
    ];

    let mut findings = BTreeMap::<_, usize>::new();

    while let Some((krate_name, run, log)) = stream.next().await {
        match log {
            Ok(log) => {
                let mut has_reason = false;

                for (target_name, target) in targets {
                    if log.contains(target) {
                        *findings.entry(target_name).or_default() += 1;
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

    run_pb.finish();

    println!("Regressed: {regressed_count}");

    for (&name, &count) in &findings {
        print!("{name}: {count}, ")
    }
    let sum: usize = findings.values().sum();
    println!("sum: {sum}, others: {}", other.len());

    println!("{other:#?}");

    Ok(())
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
            log::info!("Using cached file");
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

            log::info!(
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
