use carina_lsp::admission_coverage::AdmissionCoverage;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CoverageMode {
    AnyAdmission,
    BothPaths,
    AllSchemas,
}

pub fn validate_coverage(coverage: &AdmissionCoverage, mode: CoverageMode) -> Result<(), String> {
    let relative = coverage.relative.reached_admission;
    let expanded = coverage.expanded.reached_admission;
    if relative + expanded == 0 {
        return Err(format!("corpus reached zero admissions: {coverage:?}"));
    }
    if mode == CoverageMode::BothPaths && (relative == 0 || expanded == 0) {
        return Err(format!(
            "repository corpus must reach both admission paths: {coverage:?}"
        ));
    }
    if mode == CoverageMode::AllSchemas
        && (!coverage.relative.skipped_missing_schema.is_empty()
            || !coverage.expanded.skipped_missing_schema.is_empty())
    {
        return Err(format!(
            "infra schema coverage is incomplete, by path and provider: {coverage:?}"
        ));
    }
    Ok(())
}

#[path = "replay/artifacts.rs"]
pub mod artifacts;
#[path = "replay/manifest.rs"]
pub mod manifest;
#[path = "replay/process.rs"]
pub mod process;

use carina_core::{
    EnvironmentSnapshot, UserDirs, parser::ProviderContext, provider::ProviderFactory,
};
use carina_lsp::admission_coverage::with_admission_coverage;
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories_with_loader, install_prober};
use carina_plugin_host::{WasmLoadNotice, WasmLoadNoticeSink, WasmProviderFactory};
use manifest::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower_lsp::lsp_types::Diagnostic;

pub fn merge(total: &mut AdmissionCoverage, coverage: &AdmissionCoverage) {
    total.resolved_inputs_fell_back += coverage.resolved_inputs_fell_back;
    total.classification_preflight_ran += coverage.classification_preflight_ran;
    total.relative.reached_admission += coverage.relative.reached_admission;
    total.expanded.reached_admission += coverage.expanded.reached_admission;
    fn add<K: Ord + Clone>(total: &mut BTreeMap<K, u64>, values: &BTreeMap<K, u64>) {
        for (key, count) in values {
            *total.entry(key.clone()).or_default() += count;
        }
    }
    add(
        &mut total.relative.skipped_missing_schema,
        &coverage.relative.skipped_missing_schema,
    );
    add(
        &mut total.expanded.skipped_missing_schema,
        &coverage.expanded.skipped_missing_schema,
    );
    add(
        &mut total.expanded_declined_fell_back,
        &coverage.expanded_declined_fell_back,
    );
    add(
        &mut total.expanded_declined_skipped_module_resource,
        &coverage.expanded_declined_skipped_module_resource,
    );
}

/// Replace only root prefixes, including URL-encoded file URI spellings.
/// Array order, duplicates and every Diagnostic field survive serde unchanged.
pub fn normalize(value: &mut Value, prefixes: &[(PathBuf, &str)]) {
    let mut replacements = Vec::new();
    for (path, marker) in prefixes {
        if let Ok(uri) = tower_lsp::lsp_types::Url::from_directory_path(path) {
            replacements.push((
                uri.as_str().trim_end_matches('/').to_owned(),
                format!("file:///{marker}"),
            ));
            replacements.push((
                uri.path().trim_end_matches('/').to_owned(),
                marker.to_string(),
            ));
        }
        replacements.push((
            path.to_string_lossy().trim_end_matches('/').to_owned(),
            marker.to_string(),
        ));
    }
    // Specific nested prefixes must be replaced before their containing root.
    replacements.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
    fn walk(value: &mut Value, replacements: &[(String, String)]) {
        match value {
            Value::String(text) => {
                for (prefix, marker) in replacements {
                    let mut start = 0;
                    while let Some(offset) = text[start..].find(prefix) {
                        let offset = start + offset;
                        let end = offset + prefix.len();
                        let starts_at_root = offset == 0
                            || text[..offset].chars().next_back().is_none_or(|previous| {
                                !previous.is_alphanumeric()
                                    && !matches!(previous, '_' | '-' | '.' | '/')
                            });
                        if starts_at_root
                            && text[end..].chars().next().is_none_or(|next| {
                                !next.is_alphanumeric() && !matches!(next, '_' | '-' | '.')
                            })
                        {
                            text.replace_range(offset..end, marker);
                            start = offset + marker.len();
                        } else {
                            start = end;
                        }
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, replacements);
                }
            }
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    walk(value, replacements);
                }
            }
            _ => {}
        }
    }
    walk(value, &replacements);
}

#[derive(Serialize)]
struct OrdinalDiagnostic<'a> {
    ordinal: usize,
    #[serde(flatten)]
    diagnostic: &'a Diagnostic,
}

pub fn document_json(
    document: &str,
    diagnostics: &[Diagnostic],
    coverage: &AdmissionCoverage,
    prefixes: &[(PathBuf, &str)],
) -> Vec<u8> {
    let diagnostics = diagnostics
        .iter()
        .enumerate()
        .map(|(ordinal, diagnostic)| OrdinalDiagnostic {
            ordinal,
            diagnostic,
        })
        .collect::<Vec<_>>();
    let mut value = json!({"format": "carina-lsp-replay-v1", "document": document, "input_state": "disk; opened document only override", "coverage": coverage, "diagnostics": diagnostics});
    normalize(&mut value, prefixes);
    let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

#[derive(Default)]
struct Loads {
    providers: Vec<Value>,
    projections: Vec<Value>,
    errors: Vec<String>,
    notices: Vec<String>,
    cache_write_failures: usize,
}

fn cache_inventory(home: &Path) -> BTreeMap<String, Value> {
    let cache = UserDirs::under(home)
        .compiled_wasm_cache_dir()
        .to_path_buf();
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(cache).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "cwasm") {
            let metadata = std::fs::metadata(&path).unwrap();
            let mut value = json!({"bytes":metadata.len(),"modified_ns":metadata.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()});
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                value["inode"] = json!(metadata.ino());
            }
            files.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                value,
            );
        }
    }
    files
}

pub fn provider_prober(manifest: &Manifest) -> carina_lsp::backend::ProviderInstallProber {
    let production = install_prober();
    let root = manifest.root.clone();
    let artifacts = manifest.artifacts.clone();
    Arc::new(move |directory, config| {
        let projected = artifacts::project(
            &[(directory.to_path_buf(), config.clone())],
            &root,
            artifacts.as_deref(),
        )
        .expect("frozen replay artifact projection");
        production(&projected[0].0, &projected[0].1)
    })
}

fn replay_once(manifest: &Manifest, output: &Path) {
    assert!(
        std::env::var_os(process::CHILD).is_some(),
        "process-owned snapshots require an isolated worker"
    );
    manifest.verify();
    let start = Instant::now();
    let environment = EnvironmentSnapshot::empty();
    let infra = matches!(manifest.gate, Gate::Infra);
    let cache_before = infra.then(|| cache_inventory(&manifest.replay_home));
    let loads = Arc::new(Mutex::new(Loads::default()));
    let builder: FactoryBuilder = Arc::new({
        let loads = Arc::clone(&loads);
        let home = manifest.replay_home.clone();
        let root = manifest.root.clone();
        let artifacts = manifest.artifacts.clone();
        move |configs| {
            let projected = match artifacts::project(configs, &root, artifacts.as_deref()) {
                Ok(projected) => projected,
                Err(error) => {
                    loads.lock().unwrap().errors.push(error.clone());
                    return (
                        vec![],
                        configs
                            .iter()
                            .map(|(_, config)| (config.name.clone(), error.clone()))
                            .collect(),
                        vec![],
                    );
                }
            };
            {
                let mut loads = loads.lock().unwrap();
                for ((directory, original), (base, projected)) in configs.iter().zip(&projected) {
                    loads.projections.push(json!({"directory":directory,"provider":original.name,"configured_source":original.source.as_ref().map(ToString::to_string),"configured_version":original.version.as_ref().map(ToString::to_string),"configured_revision":original.revision,"load_directory":base,"loading_source":projected.source.as_ref().map(ToString::to_string)}));
                }
            }
            let dirs = UserDirs::under(&home);
            let grants = match artifacts
                .as_ref()
                .map_or_else(|| Ok(Default::default()), |_| artifacts::grants(&projected))
            {
                Ok(grants) => grants,
                Err(error) => {
                    loads.lock().unwrap().errors.push(error.clone());
                    return (
                        vec![],
                        configs
                            .iter()
                            .map(|(_, config)| (config.name.clone(), error.clone()))
                            .collect(),
                        vec![],
                    );
                }
            };
            let observed = Arc::clone(&loads);
            let load_directory = configs.first().map(|(directory, _)| directory.clone());
            let loader: Box<carina_lsp::factories::VerifiedFactoryLoader> = Box::new(
                move |verified, dirs, _notices| {
                    let observed = Arc::clone(&observed);
                    let dirs = dirs.clone();
                    let directory = load_directory.clone();
                    Box::pin(async move {
                        let name = verified.identity().configured_name().to_string();
                        let hash = verified.artifact().sha256().to_string();
                        let origin = format!("{:?}", verified.artifact().origin());
                        let source = format!("{:?}", verified.identity().source());
                        let notices_observed = Arc::clone(&observed);
                        let mut notices = WasmLoadNoticeSink::new(move |notice| {
                            let mut observed = notices_observed.lock().unwrap();
                            if matches!(notice, WasmLoadNotice::CacheWriteFailed(_)) {
                                observed.cache_write_failures += 1;
                                observed.errors.push(notice.to_string());
                            }
                            observed.notices.push(notice.to_string());
                        });
                        let start = Instant::now();
                        let result =
                            WasmProviderFactory::from_bytes(verified, &dirs, &mut notices).await;
                        let seconds = start.elapsed().as_secs_f64();
                        observed.lock().unwrap().providers.push(json!({"directory":directory,"provider":name,"artifact_sha256":hash,"origin":origin,"source":source,"version":result.as_ref().ok().map(|factory| factory.version()),"host_version":env!("CARGO_PKG_VERSION"),"load_seconds":seconds}));
                        result.map(|factory| Box::new(factory) as Box<dyn ProviderFactory>)
                    })
                },
            );
            let result =
                build_factories_with_loader(Ok(&dirs), &projected, &grants, loader.as_ref());
            for (name, error) in &result.1 {
                loads
                    .lock()
                    .unwrap()
                    .errors
                    .push(format!("{name}: {error}"));
            }
            result
        }
    });
    let states = tokio::sync::RwLock::new(
        ProviderStates::new(environment.clone()).with_replay_discovery(manifest.scope()),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let reload = runtime.block_on(reload_provider_states(
        &manifest.root,
        &builder,
        Some(&provider_prober(manifest)),
        &states,
    ));
    let load_seconds = start.elapsed().as_secs_f64();
    // A worker owns exactly one corpus for its entire process lifetime. The
    // parent reaps the process, releasing the provider snapshot and its native
    // metadata together; individual schema destructors are outside replay work.
    let states: &'static ProviderStates = Box::leak(Box::new(states.into_inner()));
    let context = Arc::new(ProviderContext::new(environment));
    let mut coverage = AdmissionCoverage::default();
    let mut prefixes = vec![
        (manifest.root.clone(), "$CORPUS"),
        (manifest.replay_home.clone(), "$HOME"),
        (manifest.replay_home.join("tmp"), "$TMPDIR"),
    ];
    if let Some(artifacts) = &manifest.artifacts {
        prefixes.push((artifacts.clone(), "$ARTIFACTS"));
    }
    let analysis_start = Instant::now();
    // Keep the runtime entered for production WASM configuration/type callbacks.
    let _entered = runtime.enter();
    let documents = manifest.selected_documents();
    let mut document_seconds = BTreeMap::new();
    for relative in &documents {
        let document_start = Instant::now();
        let path = manifest.root.join(relative);
        let document = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::clone(&context),
        );
        let (diagnostics, observed) =
            with_admission_coverage(|| analyze_document(states, &document, Some(&path)));
        merge(&mut coverage, &observed);
        let name = format!("{}/{}", manifest.corpus, relative.to_string_lossy());
        let destination = output.join(format!("{name}.diagnostics.json"));
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(
            destination,
            document_json(&name, &diagnostics, &observed, &prefixes),
        )
        .unwrap();
        document_seconds.insert(name, document_start.elapsed().as_secs_f64());
    }
    let analysis_seconds = analysis_start.elapsed().as_secs_f64();
    let loads = loads.lock().unwrap();
    let cache_after = infra.then(|| cache_inventory(&manifest.replay_home));
    // Timing/revision/provider metadata is deliberately outside diagnostic equality.
    write_json(
        &output.join("metrics.json"),
        &json!({"documents":documents.len(),"load_seconds":load_seconds,"analysis_seconds":analysis_seconds,"total_seconds":start.elapsed().as_secs_f64(),"schema_total":reload.schemas,"providers":loads.providers,"projections":loads.projections,"load_errors":loads.errors,"load_notices":loads.notices,"cache_write_failures":loads.cache_write_failures,"cache_before":cache_before,"cache_after":cache_after,"document_seconds":document_seconds}),
    );
    write_json(
        &output.join("provider-manifest.json"),
        &json!({"host_version":env!("CARGO_PKG_VERSION"),"providers":loads.providers,"loading_projections":loads.projections}),
    );
    write_json(
        &output.join(&manifest.corpus).join("coverage.json"),
        &serde_json::to_value(&coverage).unwrap(),
    );
    let load_errors = !loads.errors.is_empty();
    let cache_write_failures = loads.cache_write_failures;
    drop(loads);
    write_json(
        &output.join("stage.json"),
        &json!({"stage":"verify_inputs"}),
    );
    manifest.verify();
    write_json(
        &output.join("stage.json"),
        &json!({"stage":"coverage_guard"}),
    );
    let mode = match manifest.gate {
        Gate::RepositoryShard(_) => CoverageMode::AnyAdmission,
        Gate::RepositoryCoverage => CoverageMode::BothPaths,
        Gate::Infra => CoverageMode::AllSchemas,
    };
    validate_coverage(&coverage, mode).expect("admission coverage guard");
    assert_eq!(
        cache_write_failures, 0,
        "compiled cache writes failed; see metrics.json"
    );
    assert!(
        !infra || !load_errors,
        "offline provider load errors; see metrics.json"
    );
    let teardown = Instant::now();
    write_json(&output.join("stage.json"), &json!({"stage":"drop_runtime"}));
    drop(_entered);
    runtime.shutdown_background();
    write_json(
        &output.join("stage.json"),
        &json!({"stage":"complete","teardown_seconds":teardown.elapsed().as_secs_f64(),"provider_state_lifetime":"worker_process"}),
    );
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

pub const SHARDS: usize = 6;

#[derive(Serialize)]
pub struct Comparison {
    varying_documents: Vec<PathBuf>,
}

pub fn compare_paths(
    reference: &Path,
    second: &Path,
    remaining: &[PathBuf],
    corpus: &str,
    documents: &[&PathBuf],
) -> Comparison {
    let varying_documents = documents
        .iter()
        .filter(|document| {
            let name = format!("{}/{}.diagnostics.json", corpus, document.to_string_lossy());
            let reference = std::fs::read(reference.join(&name)).unwrap();
            std::iter::once(second)
                .chain(remaining.iter().map(PathBuf::as_path))
                .any(|output| std::fs::read(output.join(&name)).unwrap() != reference)
        })
        .map(|document| (*document).clone())
        .collect();
    Comparison { varying_documents }
}

// Only coverage can finish without a comparison. Determinism carries two
// mandatory completed workers independently of the number of additional runs.
enum GateResult {
    Coverage(process::CompletedWorker),
    Determinism {
        first: process::CompletedWorker,
        second: process::CompletedWorker,
        remaining: Vec<process::CompletedWorker>,
        comparison: Comparison,
    },
}

fn run_pair<F>(iterations: [usize; 2], run: &F) -> [process::CompletedWorker; 2]
where
    F: Fn(usize) -> process::CompletedWorker + Sync,
{
    std::thread::scope(|scope| {
        iterations
            .map(|iteration| scope.spawn(move || run(iteration)))
            .map(|worker| worker.join().unwrap())
    })
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    RepositoryShard(usize),
    RepositoryCoverage,
    Infra,
}

pub fn gate(test: &str, shard: usize) {
    run_gate(test, Gate::RepositoryShard(shard));
}

pub fn coverage_gate(test: &str) {
    run_gate(test, Gate::RepositoryCoverage);
}

pub fn infra_gate(test: &str) {
    run_gate(test, Gate::Infra);
}

fn run_gate(test: &str, gate: Gate) {
    if std::env::var_os(process::CHILD).is_some() {
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(std::env::var_os(process::MANIFEST).unwrap()).unwrap(),
        )
        .unwrap();
        replay_once(
            &manifest,
            Path::new(&std::env::var_os(process::WORKER_OUTPUT).unwrap()),
        );
        return;
    }
    let gate_start = Instant::now();
    let temporary = tempfile::Builder::new()
        .prefix("lsp-replay-")
        .tempdir()
        .unwrap();
    let requested = std::env::var_os("CARINA_LSP_REPLAY_OUTPUT").map(PathBuf::from);
    let output = requested.as_deref().unwrap_or(temporary.path());
    let output = match gate {
        Gate::RepositoryShard(index) => output.join(format!("shard-{index}")),
        Gate::RepositoryCoverage => output.join("coverage"),
        Gate::Infra => output.to_path_buf(),
    };
    let output = output.as_path();
    std::fs::create_dir_all(output).unwrap();
    let result = std::panic::catch_unwind(|| {
        let mut manifest = match gate {
            Gate::RepositoryShard(_) | Gate::RepositoryCoverage => Manifest::freeze_repository(
                Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
                output,
            ),
            Gate::Infra => Manifest::freeze_infra(output),
        };
        manifest.gate = gate;
        if let Gate::RepositoryShard(index) = gate {
            manifest.shard = Some((index, SHARDS));
        }
        manifest.verify();
        let manifest_path = output.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        // Warm shared compiled artifacts in one process, then bound concurrency
        // to two workers. All repetitions remain separate OS processes.
        let run = |iteration| {
            let worker_output = output.join(format!("run-{iteration:02}"));
            let infra = matches!(gate, Gate::Infra);
            let worker = process::worker(
                test,
                &manifest_path,
                &worker_output,
                &manifest.replay_home,
                Duration::from_secs(if infra { 1800 } else { 900 }),
                infra,
            );
            if infra && iteration > 0 {
                let metrics: Value = serde_json::from_slice(
                    &std::fs::read(worker.output().join("metrics.json")).unwrap(),
                )
                .unwrap();
                assert!(
                    !metrics["cache_before"].as_object().unwrap().is_empty(),
                    "infra must exercise compiled module reuse"
                );
                assert_eq!(
                    metrics["cache_before"], metrics["cache_after"],
                    "warm replay must reuse published compiled modules"
                );
            }
            worker
        };
        let documents = manifest.selected_documents();
        let result = match gate {
            Gate::RepositoryCoverage => GateResult::Coverage(run(0)),
            Gate::RepositoryShard(_) | Gate::Infra => {
                let first = run(0);
                let [second, third] = run_pair([1, 2], &run);
                let mut remaining = vec![third];
                for pair in [[3, 4], [5, 6]] {
                    remaining.extend(run_pair(pair, &run));
                }
                remaining.push(run(7));
                let outputs = remaining
                    .iter()
                    .map(|worker| worker.output().to_path_buf())
                    .collect::<Vec<_>>();
                let comparison = compare_paths(
                    first.output(),
                    second.output(),
                    &outputs,
                    &manifest.corpus,
                    &documents,
                );
                GateResult::Determinism {
                    first,
                    second,
                    remaining,
                    comparison,
                }
            }
        };
        let (workers, comparison) = match &result {
            GateResult::Coverage(worker) => (vec![worker], None),
            GateResult::Determinism {
                first,
                second,
                remaining,
                comparison,
            } => (
                std::iter::once(first)
                    .chain(std::iter::once(second))
                    .chain(remaining)
                    .collect(),
                Some(comparison),
            ),
        };
        let repetitions = workers.len();
        let durations = workers
            .iter()
            .map(|worker| worker.elapsed().as_secs_f64())
            .collect::<Vec<_>>();
        let worker_seconds_total: f64 = durations.iter().sum();
        write_json(
            &output.join("summary.json"),
            &json!({"processes":repetitions,"documents":documents.len(),"corpus_documents":manifest.documents.len(),"shard":manifest.shard,"comparison":comparison,"process_seconds":durations,"worker_seconds_total":worker_seconds_total,"gate_seconds":gate_start.elapsed().as_secs_f64()}),
        );
        match result {
            GateResult::Determinism { comparison, .. } => {
                assert!(
                    comparison.varying_documents.is_empty(),
                    "{} documents vary across {repetitions} LSP processes; evidence: {}",
                    comparison.varying_documents.len(),
                    output.display()
                );
                eprintln!(
                    "LSP replay: {repetitions} processes, {} documents, zero differences, {:.3}s total",
                    documents.len(),
                    gate_start.elapsed().as_secs_f64()
                );
            }
            GateResult::Coverage(_) => {
                eprintln!(
                    "LSP coverage: {} documents, {:.3}s total",
                    documents.len(),
                    gate_start.elapsed().as_secs_f64()
                );
            }
        }
    });
    if let Err(panic) = result {
        if requested.is_none() {
            eprintln!(
                "LSP replay evidence retained: {}",
                temporary.keep().display()
            );
        }
        std::panic::resume_unwind(panic);
    }
}
