use std::collections::BTreeMap;

use anyhow::{Result, bail};
use turbo_rcstr::rcstr;
use turbo_tasks::{ResolvedVc, Vc};
use turbo_tasks_fs::{File, FileContent, glob::Glob};
use turbopack_core::{
    asset::{Asset, AssetContent},
    chunk::{AsyncModuleInfo, ChunkableModule, ChunkingContext, EvaluatableAsset},
    ident::AssetIdent,
    module::Module,
    module_graph::ModuleGraph,
    reference::ModuleReferences,
    resolve::ModulePart,
};

use super::chunk_item::EcmascriptModuleFacadeChunkItem;
use crate::{
    AnalyzeEcmascriptModuleResult, EcmascriptAnalyzable, EcmascriptModuleContent,
    EcmascriptModuleContentOptions, SpecifiedModuleType,
    chunk::{EcmascriptChunkPlaceable, EcmascriptExports},
    code_gen::CodeGens,
    parse::ParseResult,
    references::{
        async_module::{AsyncModule, OptionAsyncModule},
        esm::{EsmExport, EsmExports, base::EsmAssetReferences},
    },
    side_effect_optimization::reference::EcmascriptModulePartReference,
    simple_tree_shake::get_module_export_usages,
};

/// A module derived from an original ecmascript module that only contains all
/// the reexports from that module and also reexports the locals from
/// [EcmascriptModuleLocalsModule]. It allows to follow
#[turbo_tasks::value]
pub struct EcmascriptModuleFacadeModule {
    module: ResolvedVc<Box<dyn EcmascriptChunkPlaceable>>,
    part: ModulePart,
    remove_unused_exports: bool,
}

#[turbo_tasks::value_impl]
impl EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    pub fn new(
        module: ResolvedVc<Box<dyn EcmascriptChunkPlaceable>>,
        part: ModulePart,
        remove_unused_exports: bool,
    ) -> Vc<Self> {
        EcmascriptModuleFacadeModule {
            module,
            part,
            remove_unused_exports,
        }
        .cell()
    }

    #[turbo_tasks::function]
    pub async fn async_module(&self) -> Result<Vc<AsyncModule>> {
        let (import_externals, has_top_level_await) =
            if let Some(async_module) = *self.module.get_async_module().await? {
                (
                    async_module.await?.import_externals,
                    async_module.await?.has_top_level_await,
                )
            } else {
                (false, false)
            };
        Ok(AsyncModule {
            has_top_level_await,
            import_externals,
        }
        .cell())
    }
}

impl EcmascriptModuleFacadeModule {
    pub async fn specific_references(
        &self,
    ) -> Result<(
        ResolvedVc<EsmAssetReferences>,
        Vec<ResolvedVc<EcmascriptModulePartReference>>,
    )> {
        Ok(match &self.part {
            ModulePart::Evaluation => {
                let Some(module) =
                    ResolvedVc::try_sidecast::<Box<dyn EcmascriptAnalyzable>>(self.module)
                else {
                    bail!(
                        "Expected EcmascriptModuleAsset for a EcmascriptModuleFacadeModule with \
                         ModulePart::Evaluation"
                    );
                };
                let result = module.analyze().await?;
                (
                    result.esm_evaluation_references,
                    vec![
                        EcmascriptModulePartReference::new_part(
                            *self.module,
                            ModulePart::locals(),
                            self.remove_unused_exports,
                        )
                        .to_resolved()
                        .await?,
                    ],
                )
            }
            ModulePart::Exports => {
                let Some(module) =
                    ResolvedVc::try_sidecast::<Box<dyn EcmascriptAnalyzable>>(self.module)
                else {
                    bail!(
                        "Expected EcmascriptModuleAsset for a EcmascriptModuleFacadeModule with \
                         ModulePart::Exports"
                    );
                };
                let result = module.analyze().await?;
                (
                    result.esm_reexport_references,
                    vec![
                        EcmascriptModulePartReference::new_part(
                            *self.module,
                            ModulePart::locals(),
                            self.remove_unused_exports,
                        )
                        .to_resolved()
                        .await?,
                    ],
                )
            }
            ModulePart::Facade => (
                EsmAssetReferences::empty().to_resolved().await?,
                vec![
                    EcmascriptModulePartReference::new_part(
                        *self.module,
                        ModulePart::evaluation(),
                        self.remove_unused_exports,
                    )
                    .to_resolved()
                    .await?,
                    EcmascriptModulePartReference::new_part(
                        *self.module,
                        ModulePart::exports(),
                        self.remove_unused_exports,
                    )
                    .to_resolved()
                    .await?,
                ],
            ),
            ModulePart::RenamedNamespace { .. } => (
                EsmAssetReferences::empty().to_resolved().await?,
                vec![
                    EcmascriptModulePartReference::new_normal(*self.module, self.part.clone())
                        .to_resolved()
                        .await?,
                ],
            ),
            ModulePart::RenamedExport { .. } => (
                EsmAssetReferences::empty().to_resolved().await?,
                vec![
                    EcmascriptModulePartReference::new_normal(*self.module, self.part.clone())
                        .to_resolved()
                        .await?,
                ],
            ),
            _ => {
                bail!("Unexpected ModulePart for EcmascriptModuleFacadeModule");
            }
        })
    }
}

#[turbo_tasks::value_impl]
impl Module for EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    fn ident(&self) -> Vc<AssetIdent> {
        self.module.ident().with_part(self.part.clone())
    }

    #[turbo_tasks::function]
    async fn references(self: Vc<Self>) -> Result<Vc<ModuleReferences>> {
        let (esm_references, part_references) = self.await?.specific_references().await?;
        let references = esm_references
            .await?
            .iter()
            .map(|r| ResolvedVc::upcast(*r))
            .chain(part_references.iter().map(|r| ResolvedVc::upcast(*r)))
            .collect();
        Ok(Vc::cell(references))
    }

    #[turbo_tasks::function]
    async fn is_self_async(self: Vc<Self>) -> Result<Vc<bool>> {
        let async_module = self.async_module();
        let references = self.references();
        let is_self_async = async_module
            .resolve()
            .await?
            .is_self_async(references.resolve().await?)
            .resolve()
            .await?;
        Ok(is_self_async)
    }
}

#[turbo_tasks::value_impl]
impl Asset for EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    fn content(&self) -> Vc<AssetContent> {
        let f = File::from("");

        AssetContent::file(FileContent::Content(f).cell())
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptAnalyzable for EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    fn analyze(&self) -> Result<Vc<AnalyzeEcmascriptModuleResult>> {
        bail!("EcmascriptModuleFacadeModule::analyze shouldn't be called");
    }

    #[turbo_tasks::function]
    fn module_content_without_analysis(
        &self,
        _generate_source_map: bool,
    ) -> Result<Vc<EcmascriptModuleContent>> {
        bail!("EcmascriptModuleFacadeModule::module_content_without_analysis shouldn't be called");
    }

    #[turbo_tasks::function]
    async fn module_content_options(
        self: ResolvedVc<Self>,
        module_graph: ResolvedVc<ModuleGraph>,
        chunking_context: ResolvedVc<Box<dyn ChunkingContext>>,
        async_module_info: Option<ResolvedVc<AsyncModuleInfo>>,
    ) -> Result<Vc<EcmascriptModuleContentOptions>> {
        let (esm_references, part_references) = self.await?.specific_references().await?;

        let export_usage_info = if self.await?.remove_unused_exports {
            Some(
                get_module_export_usages(*module_graph, Vc::upcast(*self))
                    .to_resolved()
                    .await?,
            )
        } else {
            None
        };

        Ok(EcmascriptModuleContentOptions {
            parsed: ParseResult::empty().to_resolved().await?,
            ident: self.ident().to_resolved().await?,
            specified_module_type: SpecifiedModuleType::EcmaScript,
            module_graph,
            chunking_context,
            references: self.references().to_resolved().await?,
            esm_references,
            part_references,
            code_generation: CodeGens::empty().to_resolved().await?,
            async_module: ResolvedVc::cell(Some(self.async_module().to_resolved().await?)),
            generate_source_map: false,
            original_source_map: None,
            exports: self.get_exports().to_resolved().await?,
            async_module_info,
            export_usage_info,
        }
        .cell())
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkPlaceable for EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    async fn get_exports(&self) -> Result<Vc<EcmascriptExports>> {
        let mut exports = BTreeMap::new();
        let mut star_exports = Vec::new();

        match &self.part {
            ModulePart::Exports => {
                let EcmascriptExports::EsmExports(esm_exports) = *self.module.get_exports().await?
                else {
                    bail!(
                        "EcmascriptModuleFacadeModule must only be used on modules with EsmExports"
                    );
                };
                let esm_exports = esm_exports.await?;
                for (name, export) in &esm_exports.exports {
                    let name = name.clone();
                    match export {
                        EsmExport::LocalBinding(_, mutable) => {
                            exports.insert(
                                name.clone(),
                                EsmExport::ImportedBinding(
                                    ResolvedVc::upcast(
                                        EcmascriptModulePartReference::new_part(
                                            *self.module,
                                            ModulePart::locals(),
                                            self.remove_unused_exports,
                                        )
                                        .to_resolved()
                                        .await?,
                                    ),
                                    name,
                                    *mutable,
                                ),
                            );
                        }
                        EsmExport::ImportedNamespace(reference) => {
                            exports.insert(name, EsmExport::ImportedNamespace(*reference));
                        }
                        EsmExport::ImportedBinding(reference, imported_name, mutable) => {
                            exports.insert(
                                name,
                                EsmExport::ImportedBinding(
                                    *reference,
                                    imported_name.clone(),
                                    *mutable,
                                ),
                            );
                        }
                        EsmExport::Error => {
                            exports.insert(name, EsmExport::Error);
                        }
                    }
                }
                star_exports.extend(esm_exports.star_exports.iter().copied());
            }
            ModulePart::Facade => {
                // Reexport everything from the reexports module
                // (including default export if any)
                let EcmascriptExports::EsmExports(esm_exports) = *self.module.get_exports().await?
                else {
                    bail!(
                        "EcmascriptModuleFacadeModule must only be used on modules with EsmExports"
                    );
                };
                let esm_exports = esm_exports.await?;
                if esm_exports.exports.keys().any(|name| name == "default") {
                    exports.insert(
                        rcstr!("default"),
                        EsmExport::ImportedBinding(
                            ResolvedVc::upcast(
                                EcmascriptModulePartReference::new_part(
                                    *self.module,
                                    ModulePart::exports(),
                                    self.remove_unused_exports,
                                )
                                .to_resolved()
                                .await?,
                            ),
                            rcstr!("default"),
                            false,
                        ),
                    );
                }
                star_exports.push(ResolvedVc::upcast(
                    EcmascriptModulePartReference::new_part(
                        *self.module,
                        ModulePart::exports(),
                        self.remove_unused_exports,
                    )
                    .to_resolved()
                    .await?,
                ));
            }
            ModulePart::RenamedExport {
                original_export,
                export,
            } => {
                exports.insert(
                    export.clone(),
                    EsmExport::ImportedBinding(
                        ResolvedVc::upcast(
                            EcmascriptModulePartReference::new_normal(
                                *self.module,
                                self.part.clone(),
                            )
                            .to_resolved()
                            .await?,
                        ),
                        original_export.clone(),
                        false,
                    ),
                );
            }
            ModulePart::RenamedNamespace { export } => {
                exports.insert(
                    export.clone(),
                    EsmExport::ImportedNamespace(ResolvedVc::upcast(
                        EcmascriptModulePartReference::new_normal(*self.module, self.part.clone())
                            .to_resolved()
                            .await?,
                    )),
                );
            }
            ModulePart::Evaluation => {
                // no exports
            }
            _ => bail!("Unexpected ModulePart for EcmascriptModuleFacadeModule"),
        }

        let exports = EsmExports {
            exports,
            star_exports,
        }
        .resolved_cell();
        Ok(EcmascriptExports::EsmExports(exports).cell())
    }

    #[turbo_tasks::function]
    async fn is_marked_as_side_effect_free(
        &self,
        side_effect_free_packages: Vc<Glob>,
    ) -> Result<Vc<bool>> {
        Ok(match self.part {
            ModulePart::Evaluation | ModulePart::Facade => self
                .module
                .is_marked_as_side_effect_free(side_effect_free_packages),
            ModulePart::Exports
            | ModulePart::RenamedExport { .. }
            | ModulePart::RenamedNamespace { .. } => Vc::cell(true),
            _ => bail!("Unexpected ModulePart for EcmascriptModuleFacadeModule"),
        })
    }

    #[turbo_tasks::function]
    async fn get_async_module(self: Vc<Self>) -> Result<Vc<OptionAsyncModule>> {
        Ok(Vc::cell(Some(self.async_module().to_resolved().await?)))
    }
}

#[turbo_tasks::value_impl]
impl ChunkableModule for EcmascriptModuleFacadeModule {
    #[turbo_tasks::function]
    fn as_chunk_item(
        self: ResolvedVc<Self>,
        module_graph: ResolvedVc<ModuleGraph>,
        chunking_context: ResolvedVc<Box<dyn ChunkingContext>>,
    ) -> Result<Vc<Box<dyn turbopack_core::chunk::ChunkItem>>> {
        Ok(Vc::upcast(
            EcmascriptModuleFacadeChunkItem {
                module: self,
                module_graph,
                chunking_context,
            }
            .cell(),
        ))
    }
}

#[turbo_tasks::value_impl]
impl EvaluatableAsset for EcmascriptModuleFacadeModule {}
