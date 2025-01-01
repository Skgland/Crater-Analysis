use std::{collections::BTreeSet, env::args, path::Path};

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
enum AnalysisError {
    Reqwest(#[from] reqwest::Error),
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
}

#[tokio::main]
async fn main() -> Result<(), AnalysisError> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    let expriment = args().skip(1).next().unwrap();

    if let Err(err) = std::fs::create_dir_all(format!("./results/{expriment}/logs")) {
        log::warn!("Failed to ensure cache dir exists: {err}");
    }

    let report = get_report(&expriment).await?;

    let mut regressed = 0;
    let mut e0658 = 0;
    let mut e0658_in_git = 0;
    let mut no_space = 0;
    let mut linker_bus_error = 0;
    let mut ice = 0;
    let mut results = BTreeSet::new();
    let mut other = Vec::new();
    for krate in &report.crates {
        if krate.res == "regressed" {
            regressed += 1;
            for run in krate.runs.iter().flatten() {
                results.insert(&run.res);
                if run.res == "build-fail:unknown" {
                    let log = match get_log(&expriment, &run.log).await {
                        Ok(log) => log,
                        Err(err) => {
                            log::warn!("Failed to get log '{}': {err}", run.log);
                            continue;
                        }
                    };
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
                    if log
                        .contains("collect2: fatal error: ld terminated with signal 7 [Bus error]")
                    {
                        linker_bus_error += 1;
                        has_reason = true;
                    }

                    if log.contains("error: internal compiler error:"){
                        ice += 1;
                        has_reason = true;
                    }

                    if !has_reason {
                        other.push(&krate.name);
                    }
                }
            }
        }
    }

    println!("Regressed: {regressed}");
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
    if let Err(err) = std::fs::create_dir_all(&&log_folder) {
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
            log::info!("Failed to access cached resuls, falling back to downloading: {err}");
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
