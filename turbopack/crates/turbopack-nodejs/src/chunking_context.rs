use anyhow::{Context, Result, bail};
use tracing::Instrument;
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::{ResolvedVc, TaskInput, TryJoinIterExt, Upcast, ValueToString, Vc};
use turbo_tasks_fs::FileSystemPath;
use turbopack_core::{
    asset::Asset,
    chunk::{
        Chunk, ChunkGroupResult, ChunkItem, ChunkType, ChunkableModule, ChunkingConfig,
        ChunkingConfigs, ChunkingContext, EntryChunkGroupResult, EvaluatableAssets, MinifyType,
        ModuleId, SourceMapsType,
        availability_info::AvailabilityInfo,
        chunk_group::{MakeChunkGroupResult, make_chunk_group},
        module_id_strategies::{DevModuleIdStrategy, ModuleIdStrategy},
    },
    environment::Environment,
    ident::AssetIdent,
    module::Module,
    module_graph::{ModuleGraph, chunk_group_info::ChunkGroup},
    output::{OutputAsset, OutputAssets},
};
use turbopack_ecmascript::{
    async_chunk::module::AsyncLoaderModule,
    chunk::EcmascriptChunk,
    manifest::{chunk_asset::ManifestAsyncModule, loader_item::ManifestLoaderChunkItem},
};
use turbopack_ecmascript_runtime::RuntimeType;

use crate::ecmascript::node::{
    chunk::EcmascriptBuildNodeChunk, entry::chunk::EcmascriptBuildNodeEntryChunk,
};

/// A builder for [`Vc<NodeJsChunkingContext>`].
pub struct NodeJsChunkingContextBuilder {
    chunking_context: NodeJsChunkingContext,
}

impl NodeJsChunkingContextBuilder {
    pub fn asset_prefix(mut self, asset_prefix: Option<RcStr>) -> Self {
        self.chunking_context.asset_prefix = asset_prefix;
        self
    }

    pub fn minify_type(mut self, minify_type: MinifyType) -> Self {
        self.chunking_context.minify_type = minify_type;
        self
    }

    pub fn source_maps(mut self, source_maps: SourceMapsType) -> Self {
        self.chunking_context.source_maps_type = source_maps;
        self
    }

    pub fn file_tracing(mut self, enable_tracing: bool) -> Self {
        self.chunking_context.enable_file_tracing = enable_tracing;
        self
    }

    pub fn runtime_type(mut self, runtime_type: RuntimeType) -> Self {
        self.chunking_context.runtime_type = runtime_type;
        self
    }

    pub fn manifest_chunks(mut self, manifest_chunks: bool) -> Self {
        self.chunking_context.manifest_chunks = manifest_chunks;
        self
    }

    pub fn use_file_source_map_uris(mut self) -> Self {
        self.chunking_context.should_use_file_source_map_uris = true;
        self
    }

    pub fn module_id_strategy(
        mut self,
        module_id_strategy: ResolvedVc<Box<dyn ModuleIdStrategy>>,
    ) -> Self {
        self.chunking_context.module_id_strategy = module_id_strategy;
        self
    }

    pub fn chunking_config<T>(mut self, ty: ResolvedVc<T>, chunking_config: ChunkingConfig) -> Self
    where
        T: Upcast<Box<dyn ChunkType>>,
    {
        self.chunking_context
            .chunking_configs
            .push((ResolvedVc::upcast(ty), chunking_config));
        self
    }

    /// Builds the chunking context.
    pub fn build(self) -> Vc<NodeJsChunkingContext> {
        NodeJsChunkingContext::new(self.chunking_context)
    }
}

/// A chunking context for build mode.
#[turbo_tasks::value]
#[derive(Debug, Clone, Hash, TaskInput)]
pub struct NodeJsChunkingContext {
    /// The root path of the project
    root_path: ResolvedVc<FileSystemPath>,
    /// This path is used to compute the url to request chunks or assets from
    output_root: ResolvedVc<FileSystemPath>,
    /// The relative path from the output_root to the root_path.
    output_root_to_root_path: RcStr,
    /// This path is used to compute the url to request chunks or assets from
    client_root: ResolvedVc<FileSystemPath>,
    /// Chunks are placed at this path
    chunk_root_path: ResolvedVc<FileSystemPath>,
    /// Static assets are placed at this path
    asset_root_path: ResolvedVc<FileSystemPath>,
    /// Static assets requested from this url base
    asset_prefix: Option<RcStr>,
    /// The environment chunks will be evaluated in.
    environment: ResolvedVc<Environment>,
    /// The kind of runtime to include in the output.
    runtime_type: RuntimeType,
    /// Enable tracing for this chunking
    enable_file_tracing: bool,
    /// Whether to minify resulting chunks
    minify_type: MinifyType,
    /// Whether to generate source maps
    source_maps_type: SourceMapsType,
    /// Whether to use manifest chunks for lazy compilation
    manifest_chunks: bool,
    /// The strategy to use for generating module ids
    module_id_strategy: ResolvedVc<Box<dyn ModuleIdStrategy>>,
    /// Whether to use file:// uris for source map sources
    should_use_file_source_map_uris: bool,
    /// The chunking configs
    chunking_configs: Vec<(ResolvedVc<Box<dyn ChunkType>>, ChunkingConfig)>,
}

impl NodeJsChunkingContext {
    /// Creates a new chunking context builder.
    pub fn builder(
        root_path: ResolvedVc<FileSystemPath>,
        output_root: ResolvedVc<FileSystemPath>,
        output_root_to_root_path: RcStr,
        client_root: ResolvedVc<FileSystemPath>,
        chunk_root_path: ResolvedVc<FileSystemPath>,
        asset_root_path: ResolvedVc<FileSystemPath>,
        environment: ResolvedVc<Environment>,
        runtime_type: RuntimeType,
    ) -> NodeJsChunkingContextBuilder {
        NodeJsChunkingContextBuilder {
            chunking_context: NodeJsChunkingContext {
                root_path,
                output_root,
                output_root_to_root_path,
                client_root,
                chunk_root_path,
                asset_root_path,
                asset_prefix: None,
                enable_file_tracing: false,
                environment,
                runtime_type,
                minify_type: MinifyType::NoMinify,
                source_maps_type: SourceMapsType::Full,
                manifest_chunks: false,
                should_use_file_source_map_uris: false,
                module_id_strategy: ResolvedVc::upcast(DevModuleIdStrategy::new_resolved()),
                chunking_configs: Default::default(),
            },
        }
    }
}

impl NodeJsChunkingContext {
    /// Returns the kind of runtime to include in output chunks.
    ///
    /// This is defined directly on `NodeJsChunkingContext` so it is zero-cost
    /// when `RuntimeType` has a single variant.
    pub fn runtime_type(&self) -> RuntimeType {
        self.runtime_type
    }

    /// Returns the minify type.
    pub fn minify_type(&self) -> MinifyType {
        self.minify_type
    }
}

impl NodeJsChunkingContext {
    pub async fn asset_prefix(self: Vc<Self>) -> Result<Option<RcStr>> {
        Ok(self.await?.asset_prefix.clone())
    }
}

#[turbo_tasks::value_impl]
impl NodeJsChunkingContext {
    #[turbo_tasks::function]
    fn new(this: NodeJsChunkingContext) -> Vc<Self> {
        this.cell()
    }

    #[turbo_tasks::function]
    async fn generate_chunk(
        self: Vc<Self>,
        chunk: Vc<Box<dyn Chunk>>,
    ) -> Result<Vc<Box<dyn OutputAsset>>> {
        Ok(
            if let Some(ecmascript_chunk) =
                Vc::try_resolve_downcast_type::<EcmascriptChunk>(chunk).await?
            {
                Vc::upcast(EcmascriptBuildNodeChunk::new(self, ecmascript_chunk))
            } else if let Some(output_asset) =
                Vc::try_resolve_sidecast::<Box<dyn OutputAsset>>(chunk).await?
            {
                output_asset
            } else {
                bail!("Unable to generate output asset for chunk");
            },
        )
    }
}

#[turbo_tasks::value_impl]
impl ChunkingContext for NodeJsChunkingContext {
    #[turbo_tasks::function]
    fn name(&self) -> Vc<RcStr> {
        Vc::cell(rcstr!("unknown"))
    }

    #[turbo_tasks::function]
    fn root_path(&self) -> Vc<FileSystemPath> {
        *self.root_path
    }

    #[turbo_tasks::function]
    fn output_root(&self) -> Vc<FileSystemPath> {
        *self.output_root
    }

    #[turbo_tasks::function]
    fn output_root_to_root_path(&self) -> Vc<RcStr> {
        Vc::cell(self.output_root_to_root_path.clone())
    }

    #[turbo_tasks::function]
    fn environment(&self) -> Vc<Environment> {
        *self.environment
    }

    #[turbo_tasks::function]
    fn is_tracing_enabled(&self) -> Vc<bool> {
        Vc::cell(self.enable_file_tracing)
    }

    #[turbo_tasks::function]
    pub fn minify_type(&self) -> Vc<MinifyType> {
        self.minify_type.cell()
    }

    #[turbo_tasks::function]
    async fn asset_url(&self, ident: Vc<FileSystemPath>) -> Result<Vc<RcStr>> {
        let asset_path = ident.await?.to_string();
        let asset_path = asset_path
            .strip_prefix(&format!("{}/", self.client_root.await?.path))
            .context("expected client root to contain asset path")?;

        Ok(Vc::cell(
            format!(
                "{}{}",
                self.asset_prefix.clone().unwrap_or(rcstr!("/")),
                asset_path
            )
            .into(),
        ))
    }

    #[turbo_tasks::function]
    async fn chunk_root_path(&self) -> Vc<FileSystemPath> {
        *self.chunk_root_path
    }

    #[turbo_tasks::function]
    async fn chunk_path(
        &self,
        _asset: Option<Vc<Box<dyn Asset>>>,
        ident: Vc<AssetIdent>,
        extension: RcStr,
    ) -> Result<Vc<FileSystemPath>> {
        let root_path = *self.chunk_root_path;
        let name = ident
            .output_name(*self.root_path, extension)
            .owned()
            .await?;
        Ok(root_path.join(name))
    }

    #[turbo_tasks::function]
    fn reference_chunk_source_maps(&self, _chunk: Vc<Box<dyn OutputAsset>>) -> Vc<bool> {
        Vc::cell(match self.source_maps_type {
            SourceMapsType::Full => true,
            SourceMapsType::None => false,
        })
    }

    #[turbo_tasks::function]
    fn reference_module_source_maps(&self, _module: Vc<Box<dyn Module>>) -> Vc<bool> {
        Vc::cell(match self.source_maps_type {
            SourceMapsType::Full => true,
            SourceMapsType::None => false,
        })
    }

    #[turbo_tasks::function]
    fn should_use_file_source_map_uris(&self) -> Vc<bool> {
        Vc::cell(self.should_use_file_source_map_uris)
    }

    #[turbo_tasks::function]
    async fn chunking_configs(&self) -> Result<Vc<ChunkingConfigs>> {
        Ok(Vc::cell(self.chunking_configs.iter().cloned().collect()))
    }

    #[turbo_tasks::function]
    async fn asset_path(
        &self,
        content_hash: RcStr,
        original_asset_ident: Vc<AssetIdent>,
    ) -> Result<Vc<FileSystemPath>> {
        let source_path = original_asset_ident.path().await?;
        let basename = source_path.file_name();
        let asset_path = match source_path.extension_ref() {
            Some(ext) => format!(
                "{basename}.{content_hash}.{ext}",
                basename = &basename[..basename.len() - ext.len() - 1],
                content_hash = &content_hash[..8]
            ),
            None => format!(
                "{basename}.{content_hash}",
                content_hash = &content_hash[..8]
            ),
        };
        Ok(self.asset_root_path.join(asset_path.into()))
    }

    #[turbo_tasks::function]
    async fn chunk_group(
        self: ResolvedVc<Self>,
        ident: Vc<AssetIdent>,
        chunk_group: ChunkGroup,
        module_graph: Vc<ModuleGraph>,
        availability_info: AvailabilityInfo,
    ) -> Result<Vc<ChunkGroupResult>> {
        let span = tracing::info_span!("chunking", module = ident.to_string().await?.to_string());
        async move {
            let modules = chunk_group.entries();
            let MakeChunkGroupResult {
                chunks,
                availability_info,
            } = make_chunk_group(
                modules,
                module_graph,
                ResolvedVc::upcast(self),
                availability_info,
            )
            .await?;

            let assets = chunks
                .iter()
                .map(|chunk| self.generate_chunk(**chunk).to_resolved())
                .try_join()
                .await?;

            Ok(ChunkGroupResult {
                assets: ResolvedVc::cell(assets),
                availability_info,
            }
            .cell())
        }
        .instrument(span)
        .await
    }

    #[turbo_tasks::function]
    pub async fn entry_chunk_group(
        self: ResolvedVc<Self>,
        path: Vc<FileSystemPath>,
        evaluatable_assets: Vc<EvaluatableAssets>,
        module_graph: Vc<ModuleGraph>,
        extra_chunks: Vc<OutputAssets>,
        availability_info: AvailabilityInfo,
    ) -> Result<Vc<EntryChunkGroupResult>> {
        let evaluatable_assets_ref = evaluatable_assets.await?;
        let entries = evaluatable_assets_ref
            .iter()
            .map(|&asset| ResolvedVc::upcast::<Box<dyn Module>>(asset));

        let MakeChunkGroupResult {
            chunks,
            availability_info,
        } = make_chunk_group(
            entries,
            module_graph,
            ResolvedVc::upcast(self),
            availability_info,
        )
        .await?;

        let extra_chunks = extra_chunks.await?;
        let other_chunks: Vec<_> = extra_chunks
            .iter()
            .copied()
            .chain(
                chunks
                    .iter()
                    .map(|chunk| self.generate_chunk(**chunk).to_resolved())
                    .try_join()
                    .await?,
            )
            .collect();

        let Some(module) = ResolvedVc::try_sidecast(*evaluatable_assets_ref.last().unwrap()) else {
            bail!("module must be placeable in an ecmascript chunk");
        };

        let asset = ResolvedVc::upcast(
            EcmascriptBuildNodeEntryChunk::new(
                path,
                Vc::cell(other_chunks),
                evaluatable_assets,
                *module,
                module_graph,
                *self,
            )
            .to_resolved()
            .await?,
        );

        Ok(EntryChunkGroupResult {
            asset,
            availability_info,
        }
        .cell())
    }

    #[turbo_tasks::function]
    fn evaluated_chunk_group(
        self: Vc<Self>,
        _ident: Vc<AssetIdent>,
        _chunk_group: ChunkGroup,
        _module_graph: Vc<ModuleGraph>,
        _availability_info: AvailabilityInfo,
    ) -> Result<Vc<ChunkGroupResult>> {
        // TODO(alexkirsz) This method should be part of a separate trait that is
        // only implemented for client/edge runtimes.
        bail!("the build chunking context does not support evaluated chunk groups")
    }

    #[turbo_tasks::function]
    fn chunk_item_id_from_ident(&self, ident: Vc<AssetIdent>) -> Vc<ModuleId> {
        self.module_id_strategy.get_module_id(ident)
    }

    #[turbo_tasks::function]
    async fn async_loader_chunk_item(
        self: Vc<Self>,
        module: Vc<Box<dyn ChunkableModule>>,
        module_graph: Vc<ModuleGraph>,
        availability_info: AvailabilityInfo,
    ) -> Result<Vc<Box<dyn ChunkItem>>> {
        Ok(if self.await?.manifest_chunks {
            let manifest_asset =
                ManifestAsyncModule::new(module, module_graph, Vc::upcast(self), availability_info);
            Vc::upcast(ManifestLoaderChunkItem::new(
                manifest_asset,
                module_graph,
                Vc::upcast(self),
            ))
        } else {
            let module = AsyncLoaderModule::new(module, Vc::upcast(self), availability_info);
            Vc::upcast(module.as_chunk_item(module_graph, Vc::upcast(self)))
        })
    }

    #[turbo_tasks::function]
    async fn async_loader_chunk_item_id(
        self: Vc<Self>,
        module: Vc<Box<dyn ChunkableModule>>,
    ) -> Result<Vc<ModuleId>> {
        Ok(if self.await?.manifest_chunks {
            self.chunk_item_id_from_ident(ManifestLoaderChunkItem::asset_ident_for(module))
        } else {
            self.chunk_item_id_from_ident(AsyncLoaderModule::asset_ident_for(module))
        })
    }
}
