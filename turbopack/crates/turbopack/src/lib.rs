#![feature(box_patterns)]
#![feature(trivial_bounds)]
#![feature(min_specialization)]
#![feature(map_try_insert)]
#![feature(hash_set_entry)]
#![recursion_limit = "256"]
#![feature(arbitrary_self_types)]
#![feature(arbitrary_self_types_pointers)]

pub mod evaluate_context;
pub mod global_module_ids;
mod graph;
pub mod module_options;
pub mod transition;
pub(crate) mod unsupported_sass;

use anyhow::{Result, bail};
use css::{CssModuleAsset, ModuleCssAsset};
use ecmascript::{
    EcmascriptModuleAsset, EcmascriptModuleAssetType, TreeShakingMode,
    chunk::EcmascriptChunkPlaceable,
    references::{FollowExportsResult, follow_reexports},
    side_effect_optimization::facade::module::EcmascriptModuleFacadeModule,
};
use graph::{AggregatedGraph, AggregatedGraphNodeContent, aggregate};
use module_options::{ModuleOptions, ModuleOptionsContext, ModuleRuleEffect, ModuleType};
use tracing::{Instrument, field::Empty};
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::{FxIndexSet, ResolvedVc, ValueToString, Vc};
use turbo_tasks_fs::{FileSystemPath, glob::Glob};
pub use turbopack_core::condition;
use turbopack_core::{
    asset::Asset,
    chunk::SourceMapsType,
    compile_time_info::CompileTimeInfo,
    context::{AssetContext, ProcessResult},
    environment::{Environment, ExecutionEnvironment, NodeJsEnvironment},
    issue::{IssueExt, StyledString, module::ModuleIssue},
    module::Module,
    output::OutputAsset,
    raw_module::RawModule,
    reference::{ModuleReference, TracedModuleReference},
    reference_type::{
        CssReferenceSubType, EcmaScriptModulesReferenceSubType, ImportContext, ImportWithType,
        InnerAssets, ReferenceType,
    },
    resolve::{
        ExternalTraced, ExternalType, ModulePart, ModuleResolveResult, ModuleResolveResultItem,
        ResolveResult, ResolveResultItem, options::ResolveOptions, origin::PlainResolveOrigin,
        parse::Request, resolve,
    },
    source::Source,
};
pub use turbopack_css as css;
pub use turbopack_ecmascript as ecmascript;
use turbopack_ecmascript::{
    references::external_module::{CachedExternalModule, CachedExternalType},
    tree_shake::asset::EcmascriptModulePartAsset,
};
use turbopack_json::JsonModuleAsset;
pub use turbopack_resolve::{resolve::resolve_options, resolve_options_context};
use turbopack_resolve::{resolve_options_context::ResolveOptionsContext, typescript::type_resolve};
use turbopack_static::{css::StaticUrlCssModule, ecma::StaticUrlJsModule};
use turbopack_wasm::{module_asset::WebAssemblyModuleAsset, source::WebAssemblySource};

use self::transition::{Transition, TransitionOptions};
use crate::module_options::{CssOptionsContext, CustomModuleType, EcmascriptOptionsContext};

#[turbo_tasks::function]
async fn apply_module_type(
    source: ResolvedVc<Box<dyn Source>>,
    module_asset_context: Vc<ModuleAssetContext>,
    module_type: Vc<ModuleType>,
    part: Option<ModulePart>,
    inner_assets: Option<ResolvedVc<InnerAssets>>,
    css_import_context: Option<Vc<ImportContext>>,
    runtime_code: bool,
) -> Result<Vc<ProcessResult>> {
    let module_type = &*module_type.await?;
    Ok(ProcessResult::Module(match module_type {
        ModuleType::Ecmascript {
            transforms,
            options,
        }
        | ModuleType::Typescript {
            transforms,
            tsx: _,
            analyze_types: _,
            options,
        }
        | ModuleType::TypescriptDeclaration {
            transforms,
            options,
        } => {
            let context_for_module = match module_type {
                ModuleType::Typescript { analyze_types, .. } if *analyze_types => {
                    module_asset_context.with_types_resolving_enabled()
                }
                ModuleType::TypescriptDeclaration { .. } => {
                    module_asset_context.with_types_resolving_enabled()
                }
                _ => module_asset_context,
            }
            .to_resolved()
            .await?;
            let mut builder = EcmascriptModuleAsset::builder(
                source,
                ResolvedVc::upcast(context_for_module),
                *transforms,
                *options,
                module_asset_context
                    .compile_time_info()
                    .to_resolved()
                    .await?,
            );
            match module_type {
                ModuleType::Ecmascript { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::Ecmascript)
                }
                ModuleType::Typescript {
                    tsx, analyze_types, ..
                } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::Typescript {
                        tsx: *tsx,
                        analyze_types: *analyze_types,
                    })
                }
                ModuleType::TypescriptDeclaration { .. } => {
                    builder = builder.with_type(EcmascriptModuleAssetType::TypescriptDeclaration)
                }
                _ => unreachable!(),
            }

            if let Some(inner_assets) = inner_assets {
                builder = builder.with_inner_assets(inner_assets);
            }

            let module = builder.build().to_resolved().await?;
            if runtime_code {
                ResolvedVc::upcast(module)
            } else {
                if matches!(&part, Some(ModulePart::Evaluation)) {
                    // Skip the evaluation part if the module is marked as side effect free.
                    let side_effect_free_packages = module_asset_context
                        .side_effect_free_packages()
                        .resolve()
                        .await?;

                    if *module
                        .is_marked_as_side_effect_free(side_effect_free_packages)
                        .await?
                    {
                        return Ok(ProcessResult::Ignore.cell());
                    }
                }

                let options_value = options.await?;
                match options_value.tree_shaking_mode {
                    Some(TreeShakingMode::ModuleFragments) => {
                        Vc::upcast(EcmascriptModulePartAsset::select_part(
                            *module,
                            part.unwrap_or(ModulePart::facade()),
                        ))
                    }
                    Some(TreeShakingMode::ReexportsOnly) => {
                        if let Some(part) = part {
                            match part {
                                ModulePart::Evaluation => {
                                    if *module.get_exports().needs_facade().await? {
                                        Vc::upcast(EcmascriptModuleFacadeModule::new(
                                            Vc::upcast(*module),
                                            part,
                                            options.await?.remove_unused_exports,
                                        ))
                                    } else {
                                        Vc::upcast(*module)
                                    }
                                }
                                ModulePart::Export(_) => {
                                    let side_effect_free_packages = module_asset_context
                                        .side_effect_free_packages()
                                        .resolve()
                                        .await?;

                                    if *module.get_exports().needs_facade().await? {
                                        apply_reexport_tree_shaking(
                                            Vc::upcast(
                                                EcmascriptModuleFacadeModule::new(
                                                    Vc::upcast(*module),
                                                    ModulePart::exports(),
                                                    options.await?.remove_unused_exports,
                                                )
                                                .resolve()
                                                .await?,
                                            ),
                                            part,
                                            side_effect_free_packages,
                                            options.await?.remove_unused_exports,
                                        )
                                    } else {
                                        apply_reexport_tree_shaking(
                                            Vc::upcast(*module),
                                            part,
                                            side_effect_free_packages,
                                            options.await?.remove_unused_exports,
                                        )
                                    }
                                }
                                _ => bail!(
                                    "Invalid module part \"{}\" for reexports only tree shaking \
                                     mode",
                                    part
                                ),
                            }
                        } else if *module.get_exports().needs_facade().await? {
                            Vc::upcast(EcmascriptModuleFacadeModule::new(
                                Vc::upcast(*module),
                                ModulePart::facade(),
                                options.await?.remove_unused_exports,
                            ))
                        } else {
                            Vc::upcast(*module)
                        }
                    }
                    None => Vc::upcast(*module),
                }
                .to_resolved()
                .await?
            }
        }
        ModuleType::Json => ResolvedVc::upcast(JsonModuleAsset::new(*source).to_resolved().await?),
        ModuleType::Raw => ResolvedVc::upcast(RawModule::new(*source).to_resolved().await?),
        ModuleType::CssModule => ResolvedVc::upcast(
            ModuleCssAsset::new(*source, Vc::upcast(module_asset_context))
                .to_resolved()
                .await?,
        ),

        ModuleType::Css { ty } => ResolvedVc::upcast(
            CssModuleAsset::new(
                *source,
                Vc::upcast(module_asset_context),
                *ty,
                module_asset_context
                    .module_options_context()
                    .await?
                    .css
                    .minify_type,
                css_import_context,
            )
            .to_resolved()
            .await?,
        ),
        ModuleType::StaticUrlJs => {
            ResolvedVc::upcast(StaticUrlJsModule::new(*source).to_resolved().await?)
        }
        ModuleType::StaticUrlCss => {
            ResolvedVc::upcast(StaticUrlCssModule::new(*source).to_resolved().await?)
        }
        ModuleType::WebAssembly { source_ty } => ResolvedVc::upcast(
            WebAssemblyModuleAsset::new(
                WebAssemblySource::new(*source, *source_ty),
                Vc::upcast(module_asset_context),
            )
            .to_resolved()
            .await?,
        ),
        ModuleType::Custom(custom) => {
            custom
                .create_module(*source, module_asset_context, part)
                .to_resolved()
                .await?
        }
    })
    .cell())
}

#[turbo_tasks::function]
async fn apply_reexport_tree_shaking(
    module: Vc<Box<dyn EcmascriptChunkPlaceable>>,
    part: ModulePart,
    side_effect_free_packages: Vc<Glob>,
    remove_unused_exports: bool,
) -> Result<Vc<Box<dyn Module>>> {
    if let ModulePart::Export(export) = &part {
        let FollowExportsResult {
            module: final_module,
            export_name: new_export,
            ..
        } = &*follow_reexports(module, export.clone(), side_effect_free_packages, false).await?;
        let module = if let Some(new_export) = new_export {
            if *new_export == *export {
                Vc::upcast(**final_module)
            } else {
                Vc::upcast(EcmascriptModuleFacadeModule::new(
                    **final_module,
                    ModulePart::renamed_export(new_export.clone(), export.clone()),
                    remove_unused_exports,
                ))
            }
        } else {
            Vc::upcast(EcmascriptModuleFacadeModule::new(
                **final_module,
                ModulePart::renamed_namespace(export.clone()),
                remove_unused_exports,
            ))
        };
        return Ok(module);
    }
    Ok(Vc::upcast(module))
}

#[turbo_tasks::value]
#[derive(Debug)]
pub struct ModuleAssetContext {
    pub transitions: ResolvedVc<TransitionOptions>,
    pub compile_time_info: ResolvedVc<CompileTimeInfo>,
    pub module_options_context: ResolvedVc<ModuleOptionsContext>,
    pub resolve_options_context: ResolvedVc<ResolveOptionsContext>,
    pub layer: RcStr,
    transition: Option<ResolvedVc<Box<dyn Transition>>>,
    /// Whether to replace external resolutions with CachedExternalModules. Used with
    /// ModuleOptionsContext.enable_externals_tracing to handle transitive external dependencies.
    replace_externals: bool,
}

#[turbo_tasks::value_impl]
impl ModuleAssetContext {
    #[turbo_tasks::function]
    pub fn new(
        transitions: ResolvedVc<TransitionOptions>,
        compile_time_info: ResolvedVc<CompileTimeInfo>,
        module_options_context: ResolvedVc<ModuleOptionsContext>,
        resolve_options_context: ResolvedVc<ResolveOptionsContext>,
        layer: RcStr,
    ) -> Vc<Self> {
        Self::cell(ModuleAssetContext {
            transitions,
            compile_time_info,
            module_options_context,
            resolve_options_context,
            transition: None,
            layer,
            replace_externals: true,
        })
    }

    #[turbo_tasks::function]
    pub fn new_transition(
        transitions: ResolvedVc<TransitionOptions>,
        compile_time_info: ResolvedVc<CompileTimeInfo>,
        module_options_context: ResolvedVc<ModuleOptionsContext>,
        resolve_options_context: ResolvedVc<ResolveOptionsContext>,
        layer: RcStr,
        transition: ResolvedVc<Box<dyn Transition>>,
    ) -> Vc<Self> {
        Self::cell(ModuleAssetContext {
            transitions,
            compile_time_info,
            module_options_context,
            resolve_options_context,
            layer,
            transition: Some(transition),
            replace_externals: true,
        })
    }

    #[turbo_tasks::function]
    fn new_without_replace_externals(
        transitions: ResolvedVc<TransitionOptions>,
        compile_time_info: ResolvedVc<CompileTimeInfo>,
        module_options_context: ResolvedVc<ModuleOptionsContext>,
        resolve_options_context: ResolvedVc<ResolveOptionsContext>,
        layer: RcStr,
    ) -> Vc<Self> {
        Self::cell(ModuleAssetContext {
            transitions,
            compile_time_info,
            module_options_context,
            resolve_options_context,
            transition: None,
            layer,
            replace_externals: false,
        })
    }

    #[turbo_tasks::function]
    pub fn module_options_context(&self) -> Vc<ModuleOptionsContext> {
        *self.module_options_context
    }

    #[turbo_tasks::function]
    pub fn resolve_options_context(&self) -> Vc<ResolveOptionsContext> {
        *self.resolve_options_context
    }

    #[turbo_tasks::function]
    pub async fn is_types_resolving_enabled(&self) -> Result<Vc<bool>> {
        let resolve_options_context = self.resolve_options_context.await?;
        Ok(Vc::cell(
            resolve_options_context.enable_types && resolve_options_context.enable_typescript,
        ))
    }

    #[turbo_tasks::function]
    pub async fn with_types_resolving_enabled(self: Vc<Self>) -> Result<Vc<ModuleAssetContext>> {
        if *self.is_types_resolving_enabled().await? {
            return Ok(self);
        }
        let this = self.await?;
        let resolve_options_context = this
            .resolve_options_context
            .with_types_enabled()
            .resolve()
            .await?;

        Ok(ModuleAssetContext::new(
            *this.transitions,
            *this.compile_time_info,
            *this.module_options_context,
            resolve_options_context,
            this.layer.clone(),
        ))
    }
}

impl ModuleAssetContext {
    async fn process_with_transition_rules(
        self: Vc<Self>,
        source: ResolvedVc<Box<dyn Source>>,
        reference_type: ReferenceType,
    ) -> Result<Vc<ProcessResult>> {
        let this = self.await?;
        Ok(
            if let Some(transition) = this
                .transitions
                .await?
                .get_by_rules(source, &reference_type)
                .await?
            {
                transition.process(*source, self, reference_type)
            } else {
                self.process_default(source, reference_type).await?
            },
        )
    }

    async fn process_default(
        self: Vc<Self>,
        source: ResolvedVc<Box<dyn Source>>,
        reference_type: ReferenceType,
    ) -> Result<Vc<ProcessResult>> {
        process_default(self, source, reference_type, Vec::new()).await
    }
}

async fn process_default(
    module_asset_context: Vc<ModuleAssetContext>,
    source: ResolvedVc<Box<dyn Source>>,
    reference_type: ReferenceType,
    processed_rules: Vec<usize>,
) -> Result<Vc<ProcessResult>> {
    let span = tracing::info_span!(
        "process module",
        name = %source.ident().to_string().await?,
        layer = Empty,
        reference_type = display(&reference_type)
    );
    if !span.is_disabled() {
        // Need to use record, otherwise future is not Send for some reason.
        let module_asset_context_ref = module_asset_context.await?;
        span.record("layer", module_asset_context_ref.layer.as_str());
    }
    process_default_internal(
        module_asset_context,
        source,
        reference_type,
        processed_rules,
    )
    .instrument(span)
    .await
}

async fn process_default_internal(
    module_asset_context: Vc<ModuleAssetContext>,
    source: ResolvedVc<Box<dyn Source>>,
    reference_type: ReferenceType,
    processed_rules: Vec<usize>,
) -> Result<Vc<ProcessResult>> {
    let ident = source.ident().resolve().await?;
    let path_ref = ident.path().await?;
    let options = ModuleOptions::new(
        ident.path().parent(),
        module_asset_context.module_options_context(),
        module_asset_context.resolve_options_context(),
    );

    let part: Option<ModulePart> = match &reference_type {
        ReferenceType::EcmaScriptModules(EcmaScriptModulesReferenceSubType::ImportPart(part)) => {
            Some(part.clone())
        }
        _ => None,
    };
    let inner_assets = match &reference_type {
        ReferenceType::Internal(inner_assets) => Some(*inner_assets),
        _ => None,
    };

    let mut has_type_attribute = false;

    let mut current_source = source;
    let mut current_module_type = match &reference_type {
        ReferenceType::EcmaScriptModules(EcmaScriptModulesReferenceSubType::ImportWithType(ty)) => {
            has_type_attribute = true;

            match ty {
                ImportWithType::Json => Some(ModuleType::Json),
            }
        }
        _ => None,
    };

    for (i, rule) in options.await?.rules.iter().enumerate() {
        if has_type_attribute && current_module_type.is_some() {
            continue;
        }
        if processed_rules.contains(&i) {
            continue;
        }
        if rule.matches(source, &path_ref, &reference_type).await? {
            for effect in rule.effects() {
                match effect {
                    ModuleRuleEffect::Ignore => {
                        return Ok(ProcessResult::Ignore.cell());
                    }
                    ModuleRuleEffect::SourceTransforms(transforms) => {
                        current_source =
                            transforms.transform(*current_source).to_resolved().await?;
                        if current_source.ident().resolve().await? != ident {
                            // The ident has been changed, so we need to apply new rules.
                            if let Some(transition) = module_asset_context
                                .await?
                                .transitions
                                .await?
                                .get_by_rules(current_source, &reference_type)
                                .await?
                            {
                                return Ok(transition.process(
                                    *current_source,
                                    module_asset_context,
                                    reference_type,
                                ));
                            } else {
                                let mut processed_rules = processed_rules.clone();
                                processed_rules.push(i);
                                return Box::pin(process_default(
                                    module_asset_context,
                                    current_source,
                                    reference_type,
                                    processed_rules,
                                ))
                                .await;
                            }
                        }
                    }
                    ModuleRuleEffect::ModuleType(module) => {
                        current_module_type = Some(*module);
                    }
                    ModuleRuleEffect::ExtendEcmascriptTransforms { prepend, append } => {
                        current_module_type = match current_module_type {
                            Some(ModuleType::Ecmascript {
                                transforms,
                                options,
                            }) => Some(ModuleType::Ecmascript {
                                transforms: prepend
                                    .extend(*transforms)
                                    .extend(**append)
                                    .to_resolved()
                                    .await?,
                                options,
                            }),
                            Some(ModuleType::Typescript {
                                transforms,
                                tsx,
                                analyze_types,
                                options,
                            }) => Some(ModuleType::Typescript {
                                transforms: prepend
                                    .extend(*transforms)
                                    .extend(**append)
                                    .to_resolved()
                                    .await?,
                                tsx,
                                analyze_types,
                                options,
                            }),
                            Some(module_type) => {
                                ModuleIssue {
                                    ident: ident.to_resolved().await?,
                                    title: StyledString::Text("Invalid module type".into())
                                        .resolved_cell(),
                                    description: StyledString::Text(
                                        "The module type must be Ecmascript or Typescript to add \
                                         Ecmascript transforms"
                                            .into(),
                                    )
                                    .resolved_cell(),
                                }
                                .resolved_cell()
                                .emit();
                                Some(module_type)
                            }
                            None => {
                                ModuleIssue {
                                    ident: ident.to_resolved().await?,
                                    title: StyledString::Text("Missing module type".into())
                                        .resolved_cell(),
                                    description: StyledString::Text(
                                        "The module type effect must be applied before adding \
                                         Ecmascript transforms"
                                            .into(),
                                    )
                                    .resolved_cell(),
                                }
                                .resolved_cell()
                                .emit();
                                None
                            }
                        };
                    }
                }
            }
        }
    }

    let Some(module_type) = current_module_type else {
        return Ok(ProcessResult::Unknown(current_source).cell());
    };

    Ok(apply_module_type(
        *current_source,
        module_asset_context,
        module_type.cell(),
        part,
        inner_assets.map(|v| *v),
        if let ReferenceType::Css(CssReferenceSubType::AtImport(import)) = reference_type {
            import.map(|v| *v)
        } else {
            None
        },
        matches!(reference_type, ReferenceType::Runtime),
    ))
}

#[turbo_tasks::function]
async fn externals_tracing_module_context(ty: ExternalType) -> Result<Vc<ModuleAssetContext>> {
    let env = Environment::new(ExecutionEnvironment::NodeJsLambda(
        NodeJsEnvironment::default().resolved_cell(),
    ))
    .to_resolved()
    .await?;

    let resolve_options = ResolveOptionsContext {
        emulate_environment: Some(env),
        loose_errors: true,
        custom_conditions: match ty {
            ExternalType::CommonJs => vec!["require".into()],
            ExternalType::EcmaScriptModule => vec!["import".into()],
            ExternalType::Url => vec![],
            ExternalType::Global => vec![],
        },
        ..Default::default()
    };

    Ok(ModuleAssetContext::new_without_replace_externals(
        Default::default(),
        CompileTimeInfo::builder(env).cell().await?,
        // Keep these options more or less in sync with
        // turbopack/crates/turbopack/tests/node-file-trace.rs to ensure that the NFT unit tests
        // are actually representative of what Turbopack does.
        ModuleOptionsContext {
            ecmascript: EcmascriptOptionsContext {
                source_maps: SourceMapsType::None,
                ..Default::default()
            },
            css: CssOptionsContext {
                source_maps: SourceMapsType::None,
                ..Default::default()
            },
            ..Default::default()
        }
        .cell(),
        resolve_options.cell(),
        rcstr!("externals-tracing"),
    ))
}

#[turbo_tasks::value_impl]
impl AssetContext for ModuleAssetContext {
    #[turbo_tasks::function]
    fn compile_time_info(&self) -> Vc<CompileTimeInfo> {
        *self.compile_time_info
    }

    fn layer(&self) -> RcStr {
        self.layer.clone()
    }

    #[turbo_tasks::function]
    async fn resolve_options(
        self: Vc<Self>,
        origin_path: Vc<FileSystemPath>,
        _reference_type: ReferenceType,
    ) -> Result<Vc<ResolveOptions>> {
        let this = self.await?;
        let module_asset_context = if let Some(transition) = this.transition {
            transition.process_context(self)
        } else {
            self
        };
        // TODO move `apply_commonjs/esm_resolve_options` etc. to here
        Ok(resolve_options(
            origin_path.parent().resolve().await?,
            *module_asset_context.await?.resolve_options_context,
        ))
    }

    #[turbo_tasks::function]
    async fn resolve_asset(
        self: Vc<Self>,
        origin_path: Vc<FileSystemPath>,
        request: Vc<Request>,
        resolve_options: Vc<ResolveOptions>,
        reference_type: ReferenceType,
    ) -> Result<Vc<ModuleResolveResult>> {
        let context_path = origin_path.parent().resolve().await?;

        let result = resolve(
            context_path,
            reference_type.clone(),
            request,
            resolve_options,
        );

        let mut result = self.process_resolve_result(result.resolve().await?, reference_type);

        if *self.is_types_resolving_enabled().await? {
            let types_result = type_resolve(
                Vc::upcast(PlainResolveOrigin::new(Vc::upcast(self), origin_path)),
                request,
            );

            result = ModuleResolveResult::alternatives(vec![result, types_result]);
        }

        Ok(result)
    }

    #[turbo_tasks::function]
    async fn process_resolve_result(
        self: Vc<Self>,
        result: Vc<ResolveResult>,
        reference_type: ReferenceType,
    ) -> Result<Vc<ModuleResolveResult>> {
        let this = self.await?;

        let replace_externals = this.replace_externals;
        let import_externals = this
            .module_options_context
            .await?
            .ecmascript
            .import_externals;

        let result = result.await?;

        let affecting_sources = &result.affecting_sources;

        let result = result
            .map_primary_items(|item| {
                let reference_type = reference_type.clone();
                async move {
                    Ok(match item {
                        ResolveResultItem::Source(source) => {
                            match &*self.process(*source, reference_type).await? {
                                ProcessResult::Module(module) => {
                                    ModuleResolveResultItem::Module(*module)
                                }
                                ProcessResult::Unknown(source) => {
                                    ModuleResolveResultItem::Unknown(*source)
                                }
                                ProcessResult::Ignore => ModuleResolveResultItem::Ignore,
                            }
                        }
                        ResolveResultItem::External { name, ty, traced } => {
                            let replacement = if replace_externals {
                                let additional_refs: Vec<Vc<Box<dyn ModuleReference>>> = if let (
                                    ExternalTraced::Traced,
                                    Some(tracing_root),
                                ) = (
                                    traced,
                                    self.module_options_context()
                                        .await?
                                        .enable_externals_tracing,
                                ) {
                                    let externals_context = externals_tracing_module_context(ty);
                                    let root_origin = tracing_root.join("_".into());

                                    // Normalize reference type, there is no such thing as a
                                    // `ReferenceType::EcmaScriptModules(ImportPart(Evaluation))`
                                    // for externals (and otherwise, this causes duplicate
                                    // CachedExternalModules for both `ImportPart(Evaluation)` and
                                    // `ImportPart(Export("CacheProvider"))`)
                                    let reference_type = match reference_type {
                                        ReferenceType::EcmaScriptModules(_) => {
                                            ReferenceType::EcmaScriptModules(Default::default())
                                        }
                                        ReferenceType::CommonJs(_) => {
                                            ReferenceType::CommonJs(Default::default())
                                        }
                                        ReferenceType::Css(_) => {
                                            ReferenceType::Css(Default::default())
                                        }
                                        ReferenceType::Url(_) => {
                                            ReferenceType::Url(Default::default())
                                        }
                                        _ => ReferenceType::Undefined,
                                    };

                                    let external_result = externals_context
                                        .resolve_asset(
                                            root_origin,
                                            Request::parse_string(name.clone()),
                                            externals_context.resolve_options(
                                                root_origin,
                                                reference_type.clone(),
                                            ),
                                            reference_type,
                                        )
                                        .await?;

                                    let modules = affecting_sources
                                        .iter()
                                        .chain(external_result.affecting_sources.iter())
                                        .map(|s| Vc::upcast::<Box<dyn Module>>(RawModule::new(**s)))
                                        .chain(
                                            external_result
                                                .primary_modules_raw_iter()
                                                .map(|rvc| *rvc),
                                        )
                                        .collect::<FxIndexSet<_>>();

                                    modules
                                        .into_iter()
                                        .map(|s| {
                                            Vc::upcast::<Box<dyn ModuleReference>>(
                                                TracedModuleReference::new(s),
                                            )
                                        })
                                        .collect()
                                } else {
                                    vec![]
                                };

                                replace_external(&name, ty, additional_refs, import_externals)
                                    .await?
                            } else {
                                None
                            };

                            replacement.unwrap_or_else(|| {
                                ModuleResolveResultItem::External {
                                    name,
                                    ty,
                                    // TODO(micshnic) remove that field entirely ?
                                    traced: None,
                                }
                            })
                        }
                        ResolveResultItem::Ignore => ModuleResolveResultItem::Ignore,
                        ResolveResultItem::Empty => ModuleResolveResultItem::Empty,
                        ResolveResultItem::Error(e) => ModuleResolveResultItem::Error(e),
                        ResolveResultItem::Custom(u8) => ModuleResolveResultItem::Custom(u8),
                    })
                }
            })
            .await?;

        Ok(result.cell())
    }

    #[turbo_tasks::function]
    async fn process(
        self: Vc<Self>,
        asset: ResolvedVc<Box<dyn Source>>,
        reference_type: ReferenceType,
    ) -> Result<Vc<ProcessResult>> {
        let this = self.await?;
        if let Some(transition) = this.transition {
            Ok(transition.process(*asset, self, reference_type))
        } else {
            Ok(self
                .process_with_transition_rules(asset, reference_type)
                .await?)
        }
    }

    #[turbo_tasks::function]
    async fn with_transition(&self, transition: RcStr) -> Result<Vc<Box<dyn AssetContext>>> {
        Ok(
            if let Some(transition) = self.transitions.await?.get_named(transition) {
                Vc::upcast(ModuleAssetContext::new_transition(
                    *self.transitions,
                    *self.compile_time_info,
                    *self.module_options_context,
                    *self.resolve_options_context,
                    self.layer.clone(),
                    *transition,
                ))
            } else {
                // TODO report issue
                Vc::upcast(ModuleAssetContext::new(
                    *self.transitions,
                    *self.compile_time_info,
                    *self.module_options_context,
                    *self.resolve_options_context,
                    self.layer.clone(),
                ))
            },
        )
    }

    #[turbo_tasks::function]
    async fn side_effect_free_packages(&self) -> Result<Vc<Glob>> {
        let pkgs = &*self.module_options_context.await?.side_effect_free_packages;

        let mut globs = Vec::with_capacity(pkgs.len());

        for pkg in pkgs {
            globs.push(Glob::new(format!("**/node_modules/{{{pkg}}}/**").into()));
        }

        Ok(Glob::alternatives(globs))
    }
}

#[turbo_tasks::function]
pub fn emit_with_completion(asset: Vc<Box<dyn OutputAsset>>, output_dir: Vc<FileSystemPath>) {
    let _ = emit_assets_aggregated(asset, output_dir);
}

#[turbo_tasks::function(operation)]
pub fn emit_with_completion_operation(
    asset: ResolvedVc<Box<dyn OutputAsset>>,
    output_dir: ResolvedVc<FileSystemPath>,
) -> Vc<()> {
    emit_with_completion(*asset, *output_dir)
}

#[turbo_tasks::function]
fn emit_assets_aggregated(asset: Vc<Box<dyn OutputAsset>>, output_dir: Vc<FileSystemPath>) {
    let aggregated = aggregate(asset);
    let _ = emit_aggregated_assets(aggregated, output_dir);
}

#[turbo_tasks::function]
async fn emit_aggregated_assets(
    aggregated: Vc<AggregatedGraph>,
    output_dir: Vc<FileSystemPath>,
) -> Result<()> {
    match &*aggregated.content().await? {
        AggregatedGraphNodeContent::Asset(asset) => {
            let _ = emit_asset_into_dir(**asset, output_dir);
        }
        AggregatedGraphNodeContent::Children(children) => {
            for aggregated in children {
                let _ = emit_aggregated_assets(**aggregated, output_dir);
            }
        }
    }
    Ok(())
}

#[turbo_tasks::function]
pub fn emit_asset(asset: Vc<Box<dyn OutputAsset>>) {
    let _ = asset.content().write(asset.path());
}

#[turbo_tasks::function]
pub async fn emit_asset_into_dir(
    asset: Vc<Box<dyn OutputAsset>>,
    output_dir: Vc<FileSystemPath>,
) -> Result<()> {
    let dir = &*output_dir.await?;
    if asset.path().await?.is_inside_ref(dir) {
        let _ = emit_asset(asset);
    }
    Ok(())
}

/// Replaces the externals in the result with `ExternalModuleAsset` instances.
pub async fn replace_external(
    name: &RcStr,
    ty: ExternalType,
    additional_refs: Vec<Vc<Box<dyn ModuleReference>>>,
    import_externals: bool,
) -> Result<Option<ModuleResolveResultItem>> {
    let external_type = match ty {
        ExternalType::CommonJs => CachedExternalType::CommonJs,
        ExternalType::EcmaScriptModule => {
            if import_externals {
                CachedExternalType::EcmaScriptViaImport
            } else {
                CachedExternalType::EcmaScriptViaRequire
            }
        }
        ExternalType::Global => CachedExternalType::Global,
        ExternalType::Url => {
            // we don't want to wrap url externals.
            return Ok(None);
        }
    };

    let module = CachedExternalModule::new(name.clone(), external_type, additional_refs)
        .to_resolved()
        .await?;

    Ok(Some(ModuleResolveResultItem::Module(ResolvedVc::upcast(
        module,
    ))))
}

pub fn register() {
    turbo_tasks::register();
    turbo_tasks_fs::register();
    turbopack_core::register();
    turbopack_css::register();
    turbopack_ecmascript::register();
    turbopack_node::register();
    turbopack_env::register();
    turbopack_mdx::register();
    turbopack_json::register();
    turbopack_resolve::register();
    turbopack_static::register();
    turbopack_wasm::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}
