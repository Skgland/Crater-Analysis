use std::{
    collections::{BTreeMap, HashMap},
    env::args,
    io::ErrorKind,
    path::Path,
    time::Duration,
};

use futures::StreamExt as _;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use tokio::io::BufWriter;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

#[derive(thiserror::Error, Debug)]
#[error("{0}")]
enum AnalysisError {
    Reqwest(#[from] reqwest::Error),
    Io(#[from] std::io::Error),
    Json(#[from] serde_json::Error),
    TomlDeserialization(toml::de::Error),
    #[error("Config not found")]
    MissingConfig,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
struct Config {
    crate_result: String,
    run_result: String,
    targets: HashMap<String, Vec<Target>>,
}
impl Config {
    fn example() -> Self {
        const EXAMPLE_TARGETS: &[(&str, &[&str])] = &[
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
    (
        "task or parent failed (no space)",
        &["this task or one of its parent failed: No space left on device"],
    ),
    (
        "task or parent failed (no space)",
        &["this task or one of its parent failed: Io Error: No space left on device"],
    ),
    (
        "task or parent failed (failed to clone)",
        &["this task or one of its parent failed: failed to clone"],
    ),
    ("invalid manifest", &["error: failed to parse manifest at"]),
    ("invalid manifest", &["error: invalid table header"]),
    (
        "invalid manifest",
        &["error: invalid type: ", ", expected "],
    ),
    ("invalid lockfile", &["error: failed to parse lock file at"]),
    (
        "timeout",
        &["[ERROR] error running command: no output for 300 seconds"],
    ),
    (
        "checksum mismatch",
        &["error: checksum for ", " changed between lock files"],
    ),
    (
        "links conflict",
        &[
            "the package ",
            " links to the native library ",
            ", but it conflicts with a previous package which links to ",
            " as well:",
        ],
    ),
    (
        "links conflict",
        &["error: Attempting to resolve a dependency with more than one crate with links="],
    ),
    (
        "version selection failed",
        &["error: failed to select a version for "],
    ),
    (
        "missing dep",
        &["error: no matching package named ", " found"],
    ),
    ("missing dep", &["error: no matching package found"]),
    (
        "missing dep",
        &["no matching package for override ", " found"],
    ),
    (
        "dep removed feature",
        &[
            "the package ",
            " depends on ",
            ", with features: ",
            " but ",
            " does not have these features",
        ],
    ),
    (
        "missing registry",
        &["registry index was not found in any configuration:"],
    ),
    (
        "cyclic package dependency",
        &[
            "error: cyclic package dependency: package ",
            " depends on itself. Cycle:",
        ],
    ),
    (
        "cyclic feature dependency",
        &[
            "error: cyclic feature dependency: feature ",
            " depends on itself",
        ],
    ),
    (
        "filename too long",
        &["error: unable to create ", ": File name too long"],
    ),
    ("invalid UTF-8", &["stream did not contain valid UTF-8"]),
];

        let mut targets = HashMap::<String, Vec<Target>>::new();

        for (key, all) in EXAMPLE_TARGETS {
            targets.entry(key.to_string()).or_default().push(Target {
                all: all.iter().map(|part| part.to_string()).collect(),
            });
        }

        Self {
            crate_result: "error".to_string(),
            run_result: "error".to_string(),
            targets,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
struct Target {
    all: Vec<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), AnalysisError> {
    let logger = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .build();

    let multi = MultiProgress::new();
    LogWrapper::new(multi.clone(), logger).try_init().unwrap();

    let config_path = "./analysis-config.toml";
    let config = match std::fs::read_to_string(config_path) {
        Ok(content) => match toml::from_str::<Config>(&content) {
            Ok(content) => content,
            Err(err) => {
                println!("Failed to load config '{config_path}': {err}");
                return Err(AnalysisError::TomlDeserialization(err));
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let default_config = toml::to_string_pretty(&Config::example()).unwrap();

            std::fs::write(config_path, default_config).unwrap();

            return Err(AnalysisError::MissingConfig);
        }
        Err(err) => {
            return Err(AnalysisError::Io(err));
        }
    };

    let reports = futures::stream::iter(args().skip(1))
        .map(|experiment| {
            let multi = multi.clone();
            let config = config.clone();
            async move { 
                let report_ps = multi.add(ProgressBar::new_spinner());
                let report = run_analysis(&config, &experiment, &report_ps, &multi).await?;
                report_ps.set_message(format!("Writing report for experiment {}", report.experiment));
                let path = format!("{}.report", report.experiment);
                let file = tokio::fs::File::create(&path).await?;
                let mut buffered = BufWriter::new(file);
                report.print_report(&mut buffered).await?;
                buffered.flush().await?;
                report_ps.finish_with_message(format!("Report for {} written to '{path}'", report.experiment));
                Ok(())
            }
        })
        .buffered(5)
        .collect::<Vec<Result<_,AnalysisError>>>()
        .await;

    for report in reports {
        report?;
    }

    Ok(())
}

async fn run_analysis(
    config: &Config,
    experiment: &str,
    report_ps: &ProgressBar,
    multi: &MultiProgress,
) -> Result<AnalysisReport, AnalysisError> {
    if let Err(err) = std::fs::create_dir_all(format!("./results/{experiment}/logs")) {
        log::warn!("Failed to ensure cache dir exists: {err}");
    }

    report_ps.set_message(format!("Getting Crater Report for {experiment}"));
    report_ps.enable_steady_tick(Duration::from_millis(100));
    let report = get_report(experiment).await?;
    report_ps.set_message(format!("Processing Crater Report for {experiment}"));

    let mut other = Vec::new();

    let mut regressed_count = 0;

    let interresting_runs = report
        .crates
        .iter()
        .filter(|krate| krate.res == config.crate_result)
        .inspect(|_| {
            regressed_count += 1;
        })
        .flat_map(|krate| krate.runs.iter().flatten().map(|run| (&krate.name, run)))
        .filter(|(_, run)| run.res == config.run_result)
        .collect::<Vec<_>>();

    let interesting_results_count = interresting_runs.len();
    let run_pb = multi.add(
        ProgressBar::new(interesting_results_count as u64)
            .with_message(format!("Processing logs for {experiment}")),
    );
    run_pb.set_style(
        ProgressStyle::with_template("{msg} {wide_bar} {human_pos}/{human_len}").unwrap(),
    );

    let parallelism =
        std::thread::available_parallelism().map_or(20, |available| available.get() * 2);

    let mut stream = futures::stream::iter(interresting_runs)
        .map(|(krate_name, run)| {
            let experiment = &experiment;
            async move {
                let log = get_log(experiment, &run.log).await;
                (krate_name, run, log)
            }
        })
        .buffer_unordered(parallelism);

    let mut findings = BTreeMap::<String, usize>::new();

    let error_regex = regex::RegexBuilder::new(r#"^\[INFO\] \[stdout\] error\[(E\d+)\]:"#)
        .multi_line(true)
        .build()
        .unwrap();

    while let Some((krate_name, run, log)) = stream.next().await {
        match log {
            Ok(log) => {
                let mut has_reason = false;

                for line in log.lines() {
                    for (target_name, targets) in &config.targets {
                        if targets
                            .iter()
                            .any(|target| target.all.iter().all(|pat| line.contains(pat)))
                        {
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

    run_pb.finish_and_clear();
    report_ps.finish_with_message(format!("Processed Crated Report for {experiment}"));

    Ok(AnalysisReport {
        experiment: experiment.to_string(),
        regressed_count: regressed_count,
        interesting_results_count,
        findings,
        other: other
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .fold(BTreeMap::new(), |mut acc, (krate, run)| {
                acc.entry(krate.to_string())
                    .or_default()
                    .push(run.to_string());
                acc
            }),
        expected_krate_result: config.crate_result.clone(),
        expected_run_result: config.run_result.clone(),
    })
}

struct AnalysisReport {
    experiment: String,
    expected_krate_result: String,
    expected_run_result: String,
    regressed_count: usize,
    interesting_results_count: usize,
    findings: BTreeMap<String, usize>,
    other: BTreeMap<String, Vec<String>>,
}

impl AnalysisReport {
    pub async fn print_report<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<(), std::io::Error>{
        writer.write_all(format!("Report for Crater Experiment {}\n", self.experiment).as_bytes()).await?;
        writer.write_all(format!("{} crates: {}\n", self.expected_krate_result, self.regressed_count).as_bytes()).await?;
        writer.write_all(format!("{} runs: {}\n",self.expected_run_result, self.interesting_results_count).as_bytes()).await?;
        
        writer.write_all("----------------------------------\n".as_bytes()).await?;
        writer.write_all("Results:\n".as_bytes()).await?;

        for (name, &count) in &self.findings {
            writer.write_all(format!("{name}: {count}\n").as_bytes()).await?;
        }

        let sum: usize = self.findings.values().sum();
        writer.write_all("----------------------------------\n".as_bytes()).await?;
        writer.write_all(format!("sum: {sum}\n").as_bytes()).await?;
        writer.write_all(format!("others: {}\n", self.other.len()).as_bytes()).await?;
        writer.write_all("----------------------------------\n".as_bytes()).await?;
        writer.write_all(format!("{:#?}\n", self.other).as_bytes()).await?;
        Ok(())
    }
}

async fn get_log(experiment: &str, log: &str) -> Result<String, AnalysisError> {
    let mut log_folder = format!("./results/{experiment}/logs/{log}");
    if let Some(prefix) = log_folder.strip_suffix(".") {
        let mut current = prefix.to_string() + "/dot";
        while current.contains("./") {
            current = current.replace("./", "/dot/")
        }
        log_folder = current.trim_end_matches('/').to_string();
    }

    if let Err(err) = tokio::fs::create_dir_all(&log_folder).await {
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
