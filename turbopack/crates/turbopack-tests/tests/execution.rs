#![cfg(test)]
#![feature(arbitrary_self_types)]
#![feature(arbitrary_self_types_pointers)]
#![allow(clippy::needless_return)] // tokio macro-generated code doesn't respect this

mod util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use dunce::canonicalize;
use serde::{Deserialize, Serialize};
use tracing_subscriber::{Registry, layer::SubscriberExt, util::SubscriberInitExt};
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::{
    Completion, NonLocalValue, OperationVc, ResolvedVc, TaskInput, TurboTasks, Vc, apply_effects,
    debug::ValueDebugFormat, fxindexmap, trace::TraceRawVcs,
};
use turbo_tasks_backend::{BackendOptions, TurboTasksBackend, noop_backing_storage};
use turbo_tasks_bytes::stream::SingleValue;
use turbo_tasks_env::CommandLineProcessEnv;
use turbo_tasks_fs::{
    DiskFileSystem, FileContent, FileSystem, FileSystemEntryType, FileSystemPath,
    json::parse_json_with_source_context, util::sys_to_unix,
};
use turbopack::{
    ModuleAssetContext,
    css::chunk::CssChunkType,
    ecmascript::{TreeShakingMode, chunk::EcmascriptChunkType},
    module_options::{EcmascriptOptionsContext, ModuleOptionsContext, TypescriptTransformOptions},
};
use turbopack_core::{
    chunk::ChunkingConfig,
    compile_time_defines,
    compile_time_info::CompileTimeInfo,
    condition::ContextCondition,
    context::AssetContext,
    environment::{Environment, ExecutionEnvironment, NodeJsEnvironment},
    file_source::FileSource,
    issue::IssueDescriptionExt,
    reference_type::{InnerAssets, ReferenceType},
    resolve::{
        ExternalTraced, ExternalType,
        options::{ImportMap, ImportMapping},
    },
    source::Source,
};
use turbopack_ecmascript_runtime::RuntimeType;
use turbopack_node::{debug::should_debug, evaluate::evaluate};
use turbopack_nodejs::NodeJsChunkingContext;
use turbopack_resolve::resolve_options_context::ResolveOptionsContext;
use turbopack_test_utils::{jest::JestRunResult, snapshot::UPDATE};
use turbopack_trace_utils::{
    filter_layer::FilterLayer, raw_trace::RawTraceLayer, trace_writer::TraceWriter,
    tracing_presets::TRACING_TURBO_TASKS_TARGETS,
};

use crate::util::REPO_ROOT;

#[turbo_tasks::value]
struct RunTestResult {
    js_result: ResolvedVc<JsResult>,
    path: ResolvedVc<FileSystemPath>,
}

#[turbo_tasks::value]
#[derive(Clone)]
#[serde(rename_all = "camelCase")]
struct JsResult {
    uncaught_exceptions: Vec<String>,
    unhandled_rejections: Vec<String>,
    #[turbo_tasks(trace_ignore)]
    jest_result: JestRunResult,
}

#[turbo_tasks::value]
#[derive(Copy, Clone, Debug, Hash, TaskInput)]
enum IssueSnapshotMode {
    Snapshots,
    NoSnapshots,
}

fn register() {
    turbo_tasks::register();
    turbo_tasks_env::register();
    turbo_tasks_fs::register();
    turbopack::register();
    turbopack_nodejs::register();
    turbopack_env::register();
    turbopack_ecmascript_plugins::register();
    turbopack_resolve::register();
    include!(concat!(env!("OUT_DIR"), "/register_test_execution.rs"));
}

// To minimize test path length and consistency with snapshot tests,
// node_modules is stored as a sibling of the test fixtures. Don't run
// it as a test.
//
// "Skip" directories named `__skipped__`, which include test directories to
// skip.
#[testing::fixture("tests/execution/*/*/*", exclude("node_modules|__skipped__"))]
fn test(resource: PathBuf) {
    let messages = get_messages(run(resource, IssueSnapshotMode::Snapshots).unwrap());
    if !messages.is_empty() {
        panic!(
            "Failed with error(s) in the following test(s):\n\n{}",
            messages.join("\n\n--\n")
        )
    }
}

#[testing::fixture("tests/execution/*/*/__skipped__/*/input")]
#[should_panic]
fn test_skipped_fails(resource: PathBuf) {
    let resource = resource.parent().unwrap().to_path_buf();

    let JsResult {
        // Ignore uncaught exceptions for skipped tests.
        uncaught_exceptions: _,
        unhandled_rejections: _,
        jest_result,
    } = run(resource, IssueSnapshotMode::NoSnapshots).unwrap();

    // Assert that this skipped test itself has at least one browser test which
    // fails.
    assert!(
        // Skipped tests sometimes have errors (e.g. unsupported syntax) that prevent tests from
        // running at all. Allow them to have empty results.
        jest_result.test_results.is_empty()
            || jest_result
                .test_results
                .into_iter()
                .any(|r| !r.errors.is_empty()),
    );
}

fn get_messages(js_results: JsResult) -> Vec<String> {
    let mut messages = vec![];

    if js_results.jest_result.test_results.is_empty() {
        messages.push("No tests were run.".into());
    }

    for test_result in js_results.jest_result.test_results {
        // It's possible to fail multiple tests across these tests,
        // so collect them and fail the respective test in Rust with
        // an aggregate message.
        if !test_result.errors.is_empty() {
            messages.push(format!(
                "\"{}\":\n{}",
                test_result.test_path[1..].join(" > "),
                test_result.errors.join("\n")
            ));
        }
    }

    for uncaught_exception in js_results.uncaught_exceptions {
        messages.push(format!("Uncaught exception: {uncaught_exception}"));
    }

    for unhandled_rejection in js_results.unhandled_rejections {
        messages.push(format!("Unhandled rejection: {unhandled_rejection}"));
    }

    messages
}

#[tokio::main(flavor = "current_thread")]
async fn run(resource: PathBuf, snapshot_mode: IssueSnapshotMode) -> Result<JsResult> {
    register();

    // Clean up old output files.
    let output_path = resource.join("output");
    if output_path.exists() {
        std::fs::remove_dir_all(&output_path)?;
    }

    let subscriber = Registry::default();

    let trace = TRACING_TURBO_TASKS_TARGETS.join(",");
    let subscriber = subscriber.with(FilterLayer::try_new(&trace).unwrap());

    std::fs::create_dir_all(&output_path)
        .context("Unable to create output directory")
        .unwrap();
    let trace_file = output_path.join("trace-turbopack");
    let trace_writer = std::fs::File::create(trace_file.clone()).unwrap();
    let (trace_writer, trace_writer_guard) = TraceWriter::new(trace_writer);
    let subscriber = subscriber.with(RawTraceLayer::new(trace_writer));

    subscriber.init();

    let tt = TurboTasks::new(TurboTasksBackend::new(
        BackendOptions {
            storage_mode: None,
            dependency_tracking: *UPDATE,
            ..Default::default()
        },
        noop_backing_storage(),
    ));
    let result = tt
        .run_once(async move {
            let emit_op = run_inner_operation(resource.to_str().unwrap().into(), snapshot_mode);
            let result = emit_op.read_strongly_consistent().owned().await?;
            apply_effects(emit_op).await?;

            Ok(result)
        })
        .await;

    drop(trace_writer_guard);

    result
}

#[turbo_tasks::function(operation)]
async fn run_inner_operation(
    resource: RcStr,
    snapshot_mode: IssueSnapshotMode,
) -> Result<Vc<JsResult>> {
    let prepared_test = prepare_test(resource).to_resolved().await?;
    let run_result_op = run_test_operation(prepared_test);
    if snapshot_mode == IssueSnapshotMode::Snapshots {
        snapshot_issues(*prepared_test, run_result_op).await?;
    }

    Ok(*run_result_op.connect().await?.js_result)
}

#[derive(
    PartialEq,
    Eq,
    Debug,
    Default,
    Serialize,
    Deserialize,
    TraceRawVcs,
    ValueDebugFormat,
    NonLocalValue,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TestOptions {
    tree_shaking_mode: Option<TreeShakingMode>,
    remove_unused_exports: Option<bool>,
}

#[turbo_tasks::value]
struct PreparedTest {
    path: ResolvedVc<FileSystemPath>,
    project_path: ResolvedVc<FileSystemPath>,
    tests_path: ResolvedVc<FileSystemPath>,
    project_root: ResolvedVc<FileSystemPath>,
    options: TestOptions,
}

#[turbo_tasks::function]
async fn prepare_test(resource: RcStr) -> Result<Vc<PreparedTest>> {
    let resource_path = canonicalize(&resource)?;
    assert!(resource_path.exists(), "{resource} does not exist");
    assert!(
        resource_path.is_dir(),
        "{} is not a directory. Execution tests must be directories.",
        resource_path.to_str().unwrap()
    );

    let root_fs = DiskFileSystem::new(rcstr!("workspace"), REPO_ROOT.clone(), vec![]);
    let project_fs = DiskFileSystem::new(rcstr!("project"), REPO_ROOT.clone(), vec![]);
    let project_root = project_fs.root().to_resolved().await?;

    let relative_path = resource_path.strip_prefix(&*REPO_ROOT).context(format!(
        "stripping repo root {:?} from resource path {:?}",
        &*REPO_ROOT,
        resource_path.display()
    ))?;
    let relative_path: RcStr = sys_to_unix(relative_path.to_str().unwrap()).into();
    let path = root_fs.root().join(relative_path.clone());
    let project_path = project_root.join(relative_path.clone());
    let tests_path = project_fs
        .root()
        .join(rcstr!("turbopack/crates/turbopack-tests"));

    let options_file = path.join(rcstr!("options.json"));

    let mut options = TestOptions::default();
    if matches!(*options_file.get_type().await?, FileSystemEntryType::File)
        && let FileContent::Content(content) = &*options_file.read().await?
    {
        options =
            serde_json::from_reader(content.read()).context("Unable to parse options.json")?;
    }

    Ok(PreparedTest {
        path: path.to_resolved().await?,
        project_path: project_path.to_resolved().await?,
        tests_path: tests_path.to_resolved().await?,
        project_root,
        options,
    }
    .cell())
}

#[turbo_tasks::function(operation)]
async fn run_test_operation(prepared_test: ResolvedVc<PreparedTest>) -> Result<Vc<RunTestResult>> {
    let PreparedTest {
        path,
        project_path,
        tests_path,
        project_root,
        ref options,
    } = *prepared_test.await?;

    let jest_entry_path = tests_path.join(rcstr!("js/jest-entry.ts"));
    let test_path = project_path.join(rcstr!("input/index.js"));

    let chunk_root_path = path.join(rcstr!("output")).to_resolved().await?;
    let static_root_path = path.join(rcstr!("static")).to_resolved().await?;

    let chunk_root_path_in_root_path_offset = project_path
        .join(rcstr!("output"))
        .await?
        .get_relative_path_to(&*project_root.await?)
        .context("Project path is in root path")?;

    let env = Environment::new(ExecutionEnvironment::NodeJsBuildTime(
        NodeJsEnvironment::default().resolved_cell(),
    ))
    .to_resolved()
    .await?;

    let compile_time_info = CompileTimeInfo::builder(env)
        .defines(
            compile_time_defines!(
                process.turbopack = true,
                process.env.TURBOPACK = true,
                process.env.NODE_ENV = "development",
            )
            .resolved_cell(),
        )
        .cell()
        .await?;

    let mut import_map = ImportMap::empty();
    import_map.insert_wildcard_alias(
        "esm-external/",
        ImportMapping::External(
            Some(rcstr!("*")),
            ExternalType::EcmaScriptModule,
            ExternalTraced::Untraced,
        )
        .resolved_cell(),
    );
    import_map.insert_exact_alias(
        "jest-circus",
        ImportMapping::External(None, ExternalType::CommonJs, ExternalTraced::Untraced)
            .resolved_cell(),
    );
    import_map.insert_exact_alias(
        "expect",
        ImportMapping::External(None, ExternalType::CommonJs, ExternalTraced::Untraced)
            .resolved_cell(),
    );

    let remove_unused_exports = options.remove_unused_exports.unwrap_or(true);

    let asset_context: Vc<Box<dyn AssetContext>> = Vc::upcast(ModuleAssetContext::new(
        Default::default(),
        compile_time_info,
        ModuleOptionsContext {
            ecmascript: EcmascriptOptionsContext {
                enable_typescript_transform: Some(
                    TypescriptTransformOptions::default().resolved_cell(),
                ),
                import_externals: true,
                ..Default::default()
            },
            preset_env_versions: Some(env),
            tree_shaking_mode: options.tree_shaking_mode,
            rules: vec![(
                ContextCondition::InDirectory("node_modules".into()),
                ModuleOptionsContext {
                    tree_shaking_mode: options.tree_shaking_mode,
                    remove_unused_exports,
                    ..Default::default()
                }
                .resolved_cell(),
            )],
            remove_unused_exports,
            ..Default::default()
        }
        .into(),
        ResolveOptionsContext {
            enable_typescript: true,
            enable_node_modules: Some(project_root),
            custom_conditions: vec![rcstr!("development")],
            rules: vec![(
                ContextCondition::InDirectory("node_modules".into()),
                ResolveOptionsContext {
                    enable_node_modules: Some(project_root),
                    custom_conditions: vec![rcstr!("development")],
                    browser: true,
                    ..Default::default()
                }
                .resolved_cell(),
            )],
            browser: true,
            module: true,
            import_map: Some(import_map.resolved_cell()),
            ..Default::default()
        }
        .cell(),
        rcstr!("test"),
    ));

    let chunking_context = NodeJsChunkingContext::builder(
        project_root,
        chunk_root_path,
        chunk_root_path_in_root_path_offset,
        static_root_path,
        chunk_root_path,
        static_root_path,
        env,
        RuntimeType::Development,
    )
    .chunking_config(
        Vc::<EcmascriptChunkType>::default().to_resolved().await?,
        ChunkingConfig {
            min_chunk_size: 10_000,
            ..Default::default()
        },
    )
    .chunking_config(
        Vc::<CssChunkType>::default().to_resolved().await?,
        ChunkingConfig {
            max_merge_chunk_size: 100_000,
            ..Default::default()
        },
    )
    .build();

    let jest_entry_source = FileSource::new(jest_entry_path);
    let test_source = FileSource::new(test_path);

    let test_asset = asset_context
        .process(
            Vc::upcast(test_source),
            ReferenceType::Internal(InnerAssets::empty().to_resolved().await?),
        )
        .module()
        .to_resolved()
        .await?;

    let jest_entry_asset = asset_context
        .process(
            Vc::upcast(jest_entry_source),
            ReferenceType::Internal(ResolvedVc::cell(fxindexmap! {
                rcstr!("TESTS") => test_asset,
            })),
        )
        .module();

    let res = evaluate(
        jest_entry_asset,
        *path,
        Vc::upcast(CommandLineProcessEnv::new()),
        test_source.ident(),
        asset_context,
        Vc::upcast(chunking_context),
        None,
        vec![],
        Completion::immutable(),
        should_debug("execution_test"),
    )
    .await?;

    let single = res
        .try_into_single()
        .await
        .context("test node result did not emit anything")?;

    let SingleValue::Single(bytes) = single else {
        return Ok(RunTestResult {
            js_result: JsResult {
                uncaught_exceptions: vec![],
                unhandled_rejections: vec![],
                jest_result: JestRunResult {
                    test_results: vec![],
                },
            }
            .resolved_cell(),
            path,
        }
        .cell());
    };

    Ok(RunTestResult {
        js_result: JsResult::resolved_cell(parse_json_with_source_context(bytes.to_str()?)?),
        path,
    }
    .cell())
}

#[turbo_tasks::function]
async fn snapshot_issues(
    prepared_test: Vc<PreparedTest>,
    run_result_op: OperationVc<RunTestResult>,
) -> Result<Vc<()>> {
    let PreparedTest { path, .. } = *prepared_test.await?;
    let _ = run_result_op.resolve_strongly_consistent().await;

    let captured_issues = run_result_op.peek_issues_with_path().await?;

    let plain_issues = captured_issues.get_plain_issues().await?;

    turbopack_test_utils::snapshot::snapshot_issues(
        plain_issues,
        path.join(rcstr!("issues")),
        &REPO_ROOT,
    )
    .await
    .context("Unable to handle issues")?;

    Ok(Default::default())
}
