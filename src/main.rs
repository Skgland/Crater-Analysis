use std::{collections::BTreeSet, env::args, path::Path, time::Duration};

use futures::StreamExt as _;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
// use tokio_stream::StreamExt as _;

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

    if let Err(err) = std::fs::create_dir_all(format!("./results/{expriment}/logs")) {
        log::warn!("Failed to ensure cache dir exists: {err}");
    }

    let report_ps = multi.add(ProgressBar::new_spinner().with_message("Getting Crater Report"));
    report_ps.enable_steady_tick(Duration::from_millis(100));
    let report = get_report(&expriment).await?;
    report_ps.finish_and_clear();

    let mut e0658 = 0;
    let mut e0658_in_git = 0;
    let mut no_space = 0;
    let mut linker_bus_error = 0;
    let mut ice = 0;
    let mut results = BTreeSet::new();
    let mut other = Vec::new();

    let regressed = report
        .crates
        .iter()
        .filter(|krate| krate.res == "regressed")
        .collect::<Vec<_>>();
    let runs = regressed
        .iter()
        .flat_map(|krate| krate.runs.iter().flatten().map(|run| (&krate.name, run)))
        .collect::<Vec<_>>();

    for (_, run) in &runs {
        results.insert(&run.res);
    }

    let unknown_runs = runs
        .into_iter()
        .filter(|(_, run)| run.res == "build-fail:unknown")
        .collect::<Vec<_>>();

    let run_pb = multi.add(ProgressBar::new(unknown_runs.len() as u64));
    run_pb.set_style(ProgressStyle::with_template("{wide_bar} {human_pos}/{human_len}").unwrap());

    let mut stream = tokio_stream::iter(unknown_runs)
        .map(|(krate_name, run)| {
            let expriment = &expriment;
            async move {
                let log = get_log(expriment, &run.log).await;
                (krate_name, run, log)
            }
        })
        .buffer_unordered(10);

    while let Some((krate_name, run, log)) = stream.next().await {
        match log {
            Ok(log) => {
                let mut has_reason = false;
                if log.contains(
                    "error[E0658]: use of unstable library feature `rustc_encodable_decodable`",
                ) {
                    e0658 += 1;
                    if run.log.contains("/gh/") {
                        e0658_in_git += 1;
                    }
                    has_reason = true;
                }
                if log.contains("no space left on device") {
                    no_space += 1;
                    has_reason = true;
                }
                if log.contains("collect2: fatal error: ld terminated with signal 7 [Bus error]") {
                    linker_bus_error += 1;
                    has_reason = true;
                }

                if log.contains("error: internal compiler error:") {
                    ice += 1;
                    has_reason = true;
                }

                if !has_reason {
                    other.push(krate_name);
                }
            }

            Err(err) => {
                log::warn!("Failed to get log '{}': {err}", run.log);
            }
        }

        run_pb.inc(1);
    }

    run_pb.finish();

    println!("Regressed: {}", regressed.len());
    println!("Run Results: {results:?}");
    println!(
        "E0658: {e0658}, no-space: {no_space}, linker-bus-error: {linker_bus_error}, ice: {ice}, sum: {}, other: {}",
        e0658 + no_space + linker_bus_error + ice, other.len()
    );
    println!("E0658 in Git: {e0658_in_git}");
    println!("{other:?}");

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
    let resuls = match std::fs::read_to_string(cache_path) {
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
            if let Err(err) = std::fs::write(cache_path, &content) {
                log::warn!("Failed to cache result to {cache_path:?}: {err}");
            }
            content
        }
    };
    Ok(resuls)
}
