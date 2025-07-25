use std::{fmt::Display, io::Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::{NonLocalValue, ResolvedVc, TaskInput, Vc, trace::TraceRawVcs};
use turbo_tasks_fs::{FileContent, FileSystem, VirtualFileSystem, glob::Glob, rope::RopeBuilder};
use turbopack_core::{
    asset::{Asset, AssetContent},
    chunk::{AsyncModuleInfo, ChunkItem, ChunkType, ChunkableModule, ChunkingContext},
    ident::AssetIdent,
    module::Module,
    module_graph::ModuleGraph,
    reference::{ModuleReference, ModuleReferences},
};

use crate::{
    EcmascriptModuleContent, EcmascriptOptions,
    chunk::{
        EcmascriptChunkItem, EcmascriptChunkItemContent, EcmascriptChunkPlaceable,
        EcmascriptChunkType, EcmascriptExports,
    },
    references::async_module::{AsyncModule, OptionAsyncModule},
    runtime_functions::{
        TURBOPACK_EXPORT_NAMESPACE, TURBOPACK_EXTERNAL_IMPORT, TURBOPACK_EXTERNAL_REQUIRE,
    },
    utils::StringifyJs,
};

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Serialize,
    Deserialize,
    TraceRawVcs,
    TaskInput,
    Hash,
    NonLocalValue,
)]
pub enum CachedExternalType {
    CommonJs,
    EcmaScriptViaRequire,
    EcmaScriptViaImport,
    Global,
}

impl Display for CachedExternalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CachedExternalType::CommonJs => write!(f, "cjs"),
            CachedExternalType::EcmaScriptViaRequire => write!(f, "esm_require"),
            CachedExternalType::EcmaScriptViaImport => write!(f, "esm_import"),
            CachedExternalType::Global => write!(f, "global"),
        }
    }
}

#[turbo_tasks::value]
pub struct CachedExternalModule {
    pub request: RcStr,
    pub external_type: CachedExternalType,
    pub additional_references: Vec<ResolvedVc<Box<dyn ModuleReference>>>,
}

#[turbo_tasks::value_impl]
impl CachedExternalModule {
    #[turbo_tasks::function]
    pub fn new(
        request: RcStr,
        external_type: CachedExternalType,
        additional_references: Vec<ResolvedVc<Box<dyn ModuleReference>>>,
    ) -> Vc<Self> {
        Self::cell(CachedExternalModule {
            request,
            external_type,
            additional_references,
        })
    }

    #[turbo_tasks::function]
    pub fn content(&self) -> Result<Vc<EcmascriptModuleContent>> {
        let mut code = RopeBuilder::default();

        match self.external_type {
            CachedExternalType::EcmaScriptViaImport => {
                writeln!(
                    code,
                    "const mod = await {TURBOPACK_EXTERNAL_IMPORT}({});",
                    StringifyJs(&self.request)
                )?;
            }
            CachedExternalType::Global => {
                if self.request.is_empty() {
                    writeln!(code, "const mod = {{}};")?;
                } else {
                    writeln!(
                        code,
                        "const mod = globalThis[{}];",
                        StringifyJs(&self.request)
                    )?;
                }
            }
            CachedExternalType::EcmaScriptViaRequire | CachedExternalType::CommonJs => {
                writeln!(
                    code,
                    "const mod = {TURBOPACK_EXTERNAL_REQUIRE}({}, () => require({}));",
                    StringifyJs(&self.request),
                    StringifyJs(&self.request)
                )?;
            }
        }

        writeln!(code)?;

        if self.external_type == CachedExternalType::CommonJs {
            writeln!(code, "module.exports = mod;")?;
        } else {
            writeln!(code, "{TURBOPACK_EXPORT_NAMESPACE}(mod);")?;
        }

        Ok(EcmascriptModuleContent {
            inner_code: code.build(),
            source_map: None,
            is_esm: self.external_type != CachedExternalType::CommonJs,
        }
        .cell())
    }
}

#[turbo_tasks::value_impl]
impl Module for CachedExternalModule {
    #[turbo_tasks::function]
    fn ident(&self) -> Vc<AssetIdent> {
        let fs = VirtualFileSystem::new_with_name(rcstr!("externals"));

        AssetIdent::from_path(fs.root().join(self.request.clone()))
            .with_layer(rcstr!("external"))
            .with_modifier(self.request.clone())
            .with_modifier(self.external_type.to_string().into())
    }

    #[turbo_tasks::function]
    fn references(&self) -> Result<Vc<ModuleReferences>> {
        Ok(Vc::cell(self.additional_references.clone()))
    }

    #[turbo_tasks::function]
    fn is_self_async(&self) -> Result<Vc<bool>> {
        Ok(Vc::cell(
            self.external_type == CachedExternalType::EcmaScriptViaImport,
        ))
    }
}

#[turbo_tasks::value_impl]
impl Asset for CachedExternalModule {
    #[turbo_tasks::function]
    fn content(self: Vc<Self>) -> Vc<AssetContent> {
        // should be `NotFound` as this function gets called to detect source changes
        AssetContent::file(FileContent::NotFound.cell())
    }
}

#[turbo_tasks::value_impl]
impl ChunkableModule for CachedExternalModule {
    #[turbo_tasks::function]
    fn as_chunk_item(
        self: ResolvedVc<Self>,
        _module_graph: Vc<ModuleGraph>,
        chunking_context: ResolvedVc<Box<dyn ChunkingContext>>,
    ) -> Vc<Box<dyn ChunkItem>> {
        Vc::upcast(
            CachedExternalModuleChunkItem {
                module: self,
                chunking_context,
            }
            .cell(),
        )
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkPlaceable for CachedExternalModule {
    #[turbo_tasks::function]
    fn get_exports(&self) -> Vc<EcmascriptExports> {
        if self.external_type == CachedExternalType::CommonJs {
            EcmascriptExports::CommonJs.cell()
        } else {
            EcmascriptExports::DynamicNamespace.cell()
        }
    }

    #[turbo_tasks::function]
    fn get_async_module(&self) -> Vc<OptionAsyncModule> {
        Vc::cell(
            if self.external_type == CachedExternalType::EcmaScriptViaImport {
                Some(
                    AsyncModule {
                        has_top_level_await: true,
                        import_externals: true,
                    }
                    .resolved_cell(),
                )
            } else {
                None
            },
        )
    }

    #[turbo_tasks::function]
    fn is_marked_as_side_effect_free(
        self: Vc<Self>,
        _side_effect_free_packages: Vc<Glob>,
    ) -> Vc<bool> {
        Vc::cell(false)
    }
}

#[turbo_tasks::value]
pub struct CachedExternalModuleChunkItem {
    module: ResolvedVc<CachedExternalModule>,
    chunking_context: ResolvedVc<Box<dyn ChunkingContext>>,
}

// Without this wrapper, VirtualFileSystem::new_with_name always returns a new filesystem
#[turbo_tasks::function]
fn external_fs() -> Vc<VirtualFileSystem> {
    VirtualFileSystem::new_with_name(rcstr!("externals"))
}

#[turbo_tasks::value_impl]
impl ChunkItem for CachedExternalModuleChunkItem {
    #[turbo_tasks::function]
    fn asset_ident(&self) -> Vc<AssetIdent> {
        self.module.ident()
    }

    #[turbo_tasks::function]
    fn ty(self: Vc<Self>) -> Vc<Box<dyn ChunkType>> {
        Vc::upcast(Vc::<EcmascriptChunkType>::default())
    }

    #[turbo_tasks::function]
    fn module(&self) -> Vc<Box<dyn Module>> {
        Vc::upcast(*self.module)
    }

    #[turbo_tasks::function]
    fn chunking_context(&self) -> Vc<Box<dyn ChunkingContext>> {
        *self.chunking_context
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkItem for CachedExternalModuleChunkItem {
    #[turbo_tasks::function]
    fn content(self: Vc<Self>) -> Vc<EcmascriptChunkItemContent> {
        panic!("content() should not be called");
    }

    #[turbo_tasks::function]
    fn content_with_async_module_info(
        &self,
        async_module_info: Option<Vc<AsyncModuleInfo>>,
    ) -> Vc<EcmascriptChunkItemContent> {
        let async_module_options = self
            .module
            .get_async_module()
            .module_options(async_module_info);

        EcmascriptChunkItemContent::new(
            self.module.content(),
            *self.chunking_context,
            EcmascriptOptions::default().cell(),
            async_module_options,
        )
    }
}
