use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::{Display, Formatter, Write},
    future::Future,
    iter::once,
};

use anyhow::{Result, bail};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, Level};
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::{
    FxIndexMap, FxIndexSet, NonLocalValue, ReadRef, ResolvedVc, SliceMap, TaskInput,
    TryJoinIterExt, ValueToString, Vc, trace::TraceRawVcs,
};
use turbo_tasks_fs::{
    FileSystemEntryType, FileSystemPath, RealPathResult, util::normalize_request,
};

use self::{
    options::{
        ConditionValue, ImportMapResult, ResolveInPackage, ResolveIntoPackage, ResolveModules,
        ResolveModulesOptions, ResolveOptions, resolve_modules_options,
    },
    origin::{ResolveOrigin, ResolveOriginExt},
    parse::Request,
    pattern::Pattern,
    plugin::BeforeResolvePlugin,
    remap::{ExportsField, ImportsField},
};
use crate::{
    context::AssetContext,
    data_uri_source::DataUriSource,
    file_source::FileSource,
    issue::{
        IssueExt, IssueSource, module::emit_unknown_module_type_error, resolve::ResolvingIssue,
    },
    module::{Module, Modules, OptionModule},
    output::{OutputAsset, OutputAssets},
    package_json::{PackageJsonIssue, read_package_json},
    raw_module::RawModule,
    reference_type::ReferenceType,
    resolve::{
        node::{node_cjs_resolve_options, node_esm_resolve_options},
        parse::stringify_data_uri,
        pattern::{PatternMatch, read_matches},
        plugin::AfterResolvePlugin,
    },
    source::{OptionSource, Source, Sources},
};

mod alias_map;
pub mod node;
pub mod options;
pub mod origin;
pub mod parse;
pub mod pattern;
pub mod plugin;
pub(crate) mod remap;

pub use alias_map::{
    AliasMap, AliasMapIntoIter, AliasMapLookupIterator, AliasMatch, AliasPattern, AliasTemplate,
};
pub use remap::{ResolveAliasMap, SubpathValue};

use crate::{error::PrettyPrintError, issue::IssueSeverity};

#[turbo_tasks::value(shared)]
#[derive(Clone, Debug)]
pub enum ModuleResolveResultItem {
    Module(ResolvedVc<Box<dyn Module>>),
    OutputAsset(ResolvedVc<Box<dyn OutputAsset>>),
    External {
        /// uri, path, reference, etc.
        name: RcStr,
        ty: ExternalType,
        traced: Option<ResolvedVc<ModuleResolveResult>>,
    },
    /// A module could not be created (according to the rules, e.g. no module type as assigned)
    Unknown(ResolvedVc<Box<dyn Source>>),
    Ignore,
    Error(ResolvedVc<RcStr>),
    Empty,
    Custom(u8),
}

impl ModuleResolveResultItem {
    pub async fn as_module(&self) -> Result<Option<ResolvedVc<Box<dyn Module>>>> {
        Ok(match *self {
            ModuleResolveResultItem::Module(module) => Some(module),
            ModuleResolveResultItem::Unknown(source) => {
                emit_unknown_module_type_error(*source).await?;
                None
            }
            ModuleResolveResultItem::Error(_err) => {
                // TODO emit error?
                None
            }
            _ => None,
        })
    }
}

#[turbo_tasks::value]
#[derive(Debug, Clone, Default, Hash)]
pub enum ExportUsage {
    Named(RcStr),
    /// This means the whole content of the module is used.
    #[default]
    All,
    /// Only side effects are used.
    Evaluation,
}

#[turbo_tasks::value_impl]
impl ExportUsage {
    #[turbo_tasks::function]
    pub fn all() -> Vc<Self> {
        Self::All.cell()
    }

    #[turbo_tasks::function]
    pub fn evaluation() -> Vc<Self> {
        Self::Evaluation.cell()
    }

    #[turbo_tasks::function]
    pub fn named(name: RcStr) -> Vc<Self> {
        Self::Named(name).cell()
    }
}

#[turbo_tasks::value(shared)]
#[derive(Clone)]
pub struct ModuleResolveResult {
    pub primary: SliceMap<RequestKey, ModuleResolveResultItem>,
    pub affecting_sources: Box<[ResolvedVc<Box<dyn Source>>]>,
}

impl ModuleResolveResult {
    pub fn unresolvable() -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: Default::default(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn unresolvable_with_affecting_sources(
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: Default::default(),
            affecting_sources: affecting_sources.into_boxed_slice(),
        }
        .resolved_cell()
    }

    pub fn module(module: ResolvedVc<Box<dyn Module>>) -> ResolvedVc<Self> {
        Self::module_with_key(RequestKey::default(), module)
    }

    pub fn module_with_key(
        request_key: RequestKey,
        module: ResolvedVc<Box<dyn Module>>,
    ) -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: vec![(request_key, ModuleResolveResultItem::Module(module))]
                .into_boxed_slice(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn output_asset(
        request_key: RequestKey,
        output_asset: ResolvedVc<Box<dyn OutputAsset>>,
    ) -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: vec![(
                request_key,
                ModuleResolveResultItem::OutputAsset(output_asset),
            )]
            .into_boxed_slice(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn modules(
        modules: impl IntoIterator<Item = (RequestKey, ResolvedVc<Box<dyn Module>>)>,
    ) -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: modules
                .into_iter()
                .map(|(k, v)| (k, ModuleResolveResultItem::Module(v)))
                .collect(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn modules_with_affecting_sources(
        modules: impl IntoIterator<Item = (RequestKey, ResolvedVc<Box<dyn Module>>)>,
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> ResolvedVc<Self> {
        ModuleResolveResult {
            primary: modules
                .into_iter()
                .map(|(k, v)| (k, ModuleResolveResultItem::Module(v)))
                .collect(),
            affecting_sources: affecting_sources.into_boxed_slice(),
        }
        .resolved_cell()
    }
}

impl ModuleResolveResult {
    /// Returns all module results (but ignoring any errors).
    pub fn primary_modules_raw_iter(
        &self,
    ) -> impl Iterator<Item = ResolvedVc<Box<dyn Module>>> + '_ {
        self.primary.iter().filter_map(|(_, item)| match *item {
            ModuleResolveResultItem::Module(a) => Some(a),
            _ => None,
        })
    }

    pub fn affecting_sources_iter(&self) -> impl Iterator<Item = ResolvedVc<Box<dyn Source>>> + '_ {
        self.affecting_sources.iter().copied()
    }

    pub fn is_unresolvable_ref(&self) -> bool {
        self.primary.is_empty()
    }
}

pub struct ModuleResolveResultBuilder {
    pub primary: FxIndexMap<RequestKey, ModuleResolveResultItem>,
    pub affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
}

impl From<ModuleResolveResultBuilder> for ModuleResolveResult {
    fn from(v: ModuleResolveResultBuilder) -> Self {
        ModuleResolveResult {
            primary: v.primary.into_iter().collect(),
            affecting_sources: v.affecting_sources.into_boxed_slice(),
        }
    }
}
impl From<ModuleResolveResult> for ModuleResolveResultBuilder {
    fn from(v: ModuleResolveResult) -> Self {
        ModuleResolveResultBuilder {
            primary: IntoIterator::into_iter(v.primary).collect(),
            affecting_sources: v.affecting_sources.into_vec(),
        }
    }
}
impl ModuleResolveResultBuilder {
    pub fn merge_alternatives(&mut self, other: &ModuleResolveResult) {
        for (k, v) in other.primary.iter() {
            if !self.primary.contains_key(k) {
                self.primary.insert(k.clone(), v.clone());
            }
        }
        let set = self
            .affecting_sources
            .iter()
            .copied()
            .collect::<FxHashSet<_>>();
        self.affecting_sources.extend(
            other
                .affecting_sources
                .iter()
                .filter(|source| !set.contains(source))
                .copied(),
        );
    }
}

#[turbo_tasks::value_impl]
impl ModuleResolveResult {
    #[turbo_tasks::function]
    pub async fn with_affecting_source(
        &self,
        source: ResolvedVc<Box<dyn Source>>,
    ) -> Result<Vc<Self>> {
        Ok(Self {
            primary: self.primary.clone(),
            affecting_sources: self
                .affecting_sources
                .iter()
                .copied()
                .chain(std::iter::once(source))
                .collect(),
        }
        .cell())
    }

    #[turbo_tasks::function]
    pub async fn with_affecting_sources(
        &self,
        sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> Result<Vc<Self>> {
        Ok(Self {
            primary: self.primary.clone(),
            affecting_sources: self
                .affecting_sources
                .iter()
                .copied()
                .chain(sources)
                .collect(),
        }
        .cell())
    }

    /// Returns the first [ModuleResolveResult] that is not
    /// [ModuleResolveResult::Unresolvable] in the given list, while keeping
    /// track of all the affecting_sources in all the [ModuleResolveResult]s.
    #[turbo_tasks::function]
    pub async fn select_first(results: Vec<Vc<ModuleResolveResult>>) -> Result<Vc<Self>> {
        let mut affecting_sources = vec![];
        for result in &results {
            affecting_sources.extend(result.await?.affecting_sources_iter());
        }
        for result in results {
            let result_ref = result.await?;
            if !result_ref.is_unresolvable_ref() {
                return Ok(Self {
                    primary: result_ref.primary.clone(),
                    affecting_sources: affecting_sources.into_boxed_slice(),
                }
                .cell());
            }
        }
        Ok(*ModuleResolveResult::unresolvable_with_affecting_sources(
            affecting_sources,
        ))
    }

    #[turbo_tasks::function]
    pub async fn alternatives(results: Vec<Vc<ModuleResolveResult>>) -> Result<Vc<Self>> {
        if results.len() == 1 {
            return Ok(results.into_iter().next().unwrap());
        }
        let mut iter = results.into_iter().try_join().await?.into_iter();
        if let Some(current) = iter.next() {
            let mut current: ModuleResolveResultBuilder = ReadRef::into_owned(current).into();
            for result in iter {
                // For clippy -- This explicit deref is necessary
                let other = &*result;
                current.merge_alternatives(other);
            }
            Ok(Self::cell(current.into()))
        } else {
            Ok(*ModuleResolveResult::unresolvable())
        }
    }

    #[turbo_tasks::function]
    pub async fn alternatives_with_affecting_sources(
        results: Vec<Vc<ModuleResolveResult>>,
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> Result<Vc<Self>> {
        if affecting_sources.is_empty() {
            return Ok(Self::alternatives(results));
        }
        if results.len() == 1 {
            return Ok(results
                .into_iter()
                .next()
                .unwrap()
                .with_affecting_sources(affecting_sources.into_iter().map(|src| *src).collect()));
        }
        let mut iter = results.into_iter().try_join().await?.into_iter();
        if let Some(current) = iter.next() {
            let mut current: ModuleResolveResultBuilder = ReadRef::into_owned(current).into();
            for result in iter {
                // For clippy -- This explicit deref is necessary
                let other = &*result;
                current.merge_alternatives(other);
            }
            current.affecting_sources.extend(affecting_sources);
            Ok(Self::cell(current.into()))
        } else {
            Ok(*ModuleResolveResult::unresolvable_with_affecting_sources(
                affecting_sources,
            ))
        }
    }

    #[turbo_tasks::function]
    pub fn is_unresolvable(&self) -> Vc<bool> {
        Vc::cell(self.is_unresolvable_ref())
    }

    #[turbo_tasks::function]
    pub async fn first_module(&self) -> Result<Vc<OptionModule>> {
        for (_, item) in self.primary.iter() {
            if let Some(module) = item.as_module().await? {
                return Ok(Vc::cell(Some(module)));
            }
        }
        Ok(Vc::cell(None))
    }

    /// Returns a set (no duplicates) of primary modules in the result. All
    /// modules are already resolved Vc.
    #[turbo_tasks::function]
    pub async fn primary_modules(&self) -> Result<Vc<Modules>> {
        let mut set = FxIndexSet::default();
        for (_, item) in self.primary.iter() {
            if let Some(module) = item.as_module().await? {
                set.insert(module);
            }
        }
        Ok(Vc::cell(set.into_iter().collect()))
    }

    #[turbo_tasks::function]
    pub fn primary_output_assets(&self) -> Vc<OutputAssets> {
        Vc::cell(
            self.primary
                .iter()
                .filter_map(|(_, item)| match item {
                    &ModuleResolveResultItem::OutputAsset(a) => Some(a),
                    _ => None,
                })
                .collect(),
        )
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    TaskInput,
    Hash,
    NonLocalValue,
    TraceRawVcs,
    Serialize,
    Deserialize,
)]
pub enum ExternalTraced {
    Untraced,
    Traced,
}

impl Display for ExternalTraced {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ExternalTraced::Untraced => write!(f, "untraced"),
            ExternalTraced::Traced => write!(f, "traced"),
        }
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Serialize,
    Deserialize,
    TraceRawVcs,
    TaskInput,
    NonLocalValue,
)]
pub enum ExternalType {
    Url,
    CommonJs,
    EcmaScriptModule,
    Global,
}

impl Display for ExternalType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ExternalType::CommonJs => write!(f, "commonjs"),
            ExternalType::EcmaScriptModule => write!(f, "esm"),
            ExternalType::Url => write!(f, "url"),
            ExternalType::Global => write!(f, "global"),
        }
    }
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone)]
pub enum ResolveResultItem {
    Source(ResolvedVc<Box<dyn Source>>),
    External {
        /// uri, path, reference, etc.
        name: RcStr,
        ty: ExternalType,
        traced: ExternalTraced,
    },
    Ignore,
    Error(ResolvedVc<RcStr>),
    Empty,
    Custom(u8),
}

/// Represents the key for a request that leads to a certain results during
/// resolving.
///
/// A primary factor is the actual request string, but there are
/// other factors like exports conditions that can affect resolting and become
/// part of the key (assuming the condition is unknown at compile time)
#[derive(Clone, Debug, Default, Hash, TaskInput)]
#[turbo_tasks::value]
pub struct RequestKey {
    pub request: Option<RcStr>,
    pub conditions: BTreeMap<String, bool>,
}

impl Display for RequestKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(request) = &self.request {
            write!(f, "{request}")?;
        } else {
            write!(f, "<default>")?;
        }
        if !self.conditions.is_empty() {
            write!(f, " (")?;
            for (i, (k, v)) in self.conditions.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{k}={v}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

impl RequestKey {
    pub fn new(request: RcStr) -> Self {
        RequestKey {
            request: Some(request),
            ..Default::default()
        }
    }
}

#[turbo_tasks::value(shared)]
#[derive(Clone)]
pub struct ResolveResult {
    pub primary: SliceMap<RequestKey, ResolveResultItem>,
    pub affecting_sources: Box<[ResolvedVc<Box<dyn Source>>]>,
}

#[turbo_tasks::value_impl]
impl ValueToString for ResolveResult {
    #[turbo_tasks::function]
    async fn to_string(&self) -> Result<Vc<RcStr>> {
        let mut result = String::new();
        if self.is_unresolvable_ref() {
            result.push_str("unresolvable");
        }
        for (i, (request, item)) in self.primary.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            write!(result, "{request} -> ").unwrap();
            match item {
                ResolveResultItem::Source(a) => {
                    result.push_str(&a.ident().to_string().await?);
                }
                ResolveResultItem::External {
                    name: s,
                    ty,
                    traced,
                } => {
                    result.push_str("external ");
                    result.push_str(s);
                    write!(result, " ({ty}, {traced})")?;
                }
                ResolveResultItem::Ignore => {
                    result.push_str("ignore");
                }
                ResolveResultItem::Empty => {
                    result.push_str("empty");
                }
                ResolveResultItem::Error(_) => {
                    result.push_str("error");
                }
                ResolveResultItem::Custom(_) => {
                    result.push_str("custom");
                }
            }
            result.push('\n');
        }
        if !self.affecting_sources.is_empty() {
            result.push_str(" (affecting sources: ");
            for (i, source) in self.affecting_sources.iter().enumerate() {
                if i > 0 {
                    result.push_str(", ");
                }
                result.push_str(&source.ident().to_string().await?);
            }
            result.push(')');
        }
        Ok(Vc::cell(result.into()))
    }
}

impl ResolveResult {
    pub fn unresolvable() -> ResolvedVc<Self> {
        ResolveResult {
            primary: Default::default(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn unresolvable_with_affecting_sources(
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> ResolvedVc<Self> {
        ResolveResult {
            primary: Default::default(),
            affecting_sources: affecting_sources.into_boxed_slice(),
        }
        .resolved_cell()
    }

    pub fn primary(result: ResolveResultItem) -> ResolvedVc<Self> {
        Self::primary_with_key(RequestKey::default(), result)
    }

    pub fn primary_with_key(
        request_key: RequestKey,
        result: ResolveResultItem,
    ) -> ResolvedVc<Self> {
        ResolveResult {
            primary: vec![(request_key, result)].into_boxed_slice(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn primary_with_affecting_sources(
        request_key: RequestKey,
        result: ResolveResultItem,
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> ResolvedVc<Self> {
        ResolveResult {
            primary: vec![(request_key, result)].into_boxed_slice(),
            affecting_sources: affecting_sources.into_boxed_slice(),
        }
        .resolved_cell()
    }

    pub fn source(source: ResolvedVc<Box<dyn Source>>) -> ResolvedVc<Self> {
        Self::source_with_key(RequestKey::default(), source)
    }

    pub fn source_with_key(
        request_key: RequestKey,
        source: ResolvedVc<Box<dyn Source>>,
    ) -> ResolvedVc<Self> {
        ResolveResult {
            primary: vec![(request_key, ResolveResultItem::Source(source))].into_boxed_slice(),
            affecting_sources: Default::default(),
        }
        .resolved_cell()
    }

    pub fn source_with_affecting_sources(
        request_key: RequestKey,
        source: ResolvedVc<Box<dyn Source>>,
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> ResolvedVc<Self> {
        ResolveResult {
            primary: vec![(request_key, ResolveResultItem::Source(source))].into_boxed_slice(),
            affecting_sources: affecting_sources.into_boxed_slice(),
        }
        .resolved_cell()
    }
}

impl ResolveResult {
    pub fn get_affecting_sources(&self) -> impl Iterator<Item = ResolvedVc<Box<dyn Source>>> + '_ {
        self.affecting_sources.iter().copied()
    }

    pub fn is_unresolvable_ref(&self) -> bool {
        self.primary.is_empty()
    }

    pub async fn map<A, AF, R, RF>(&self, source_fn: A, affecting_source_fn: R) -> Result<Self>
    where
        A: Fn(ResolvedVc<Box<dyn Source>>) -> AF,
        AF: Future<Output = Result<ResolvedVc<Box<dyn Source>>>>,
        R: Fn(ResolvedVc<Box<dyn Source>>) -> RF,
        RF: Future<Output = Result<ResolvedVc<Box<dyn Source>>>>,
    {
        Ok(Self {
            primary: self
                .primary
                .iter()
                .map(|(request, result)| {
                    let asset_fn = &source_fn;
                    let request = request.clone();
                    let result = result.clone();
                    async move {
                        if let ResolveResultItem::Source(asset) = result {
                            Ok((request, ResolveResultItem::Source(asset_fn(asset).await?)))
                        } else {
                            Ok((request, result))
                        }
                    }
                })
                .try_join()
                .await?
                .into_iter()
                .collect(),
            affecting_sources: self
                .affecting_sources
                .iter()
                .copied()
                .map(affecting_source_fn)
                .try_join()
                .await?
                .into_boxed_slice(),
        })
    }

    pub async fn map_module<A, AF>(&self, source_fn: A) -> Result<ModuleResolveResult>
    where
        A: Fn(ResolvedVc<Box<dyn Source>>) -> AF,
        AF: Future<Output = Result<ModuleResolveResultItem>>,
    {
        Ok(ModuleResolveResult {
            primary: self
                .primary
                .iter()
                .map(|(request, item)| {
                    let asset_fn = &source_fn;
                    let request = request.clone();
                    let item = item.clone();
                    async move {
                        Ok((
                            request,
                            match item {
                                ResolveResultItem::Source(source) => asset_fn(source).await?,
                                ResolveResultItem::External { name, ty, traced } => {
                                    if traced == ExternalTraced::Traced {
                                        // Should use map_primary_items instead
                                        bail!("map_module doesn't handle traced externals");
                                    }
                                    ModuleResolveResultItem::External {
                                        name,
                                        ty,
                                        traced: None,
                                    }
                                }
                                ResolveResultItem::Ignore => ModuleResolveResultItem::Ignore,
                                ResolveResultItem::Empty => ModuleResolveResultItem::Empty,
                                ResolveResultItem::Error(e) => ModuleResolveResultItem::Error(e),
                                ResolveResultItem::Custom(u8) => {
                                    ModuleResolveResultItem::Custom(u8)
                                }
                            },
                        ))
                    }
                })
                .try_join()
                .await?
                .into_iter()
                .collect(),
            affecting_sources: self.affecting_sources.clone(),
        })
    }

    pub async fn map_primary_items<A, AF>(&self, item_fn: A) -> Result<ModuleResolveResult>
    where
        A: Fn(ResolveResultItem) -> AF,
        AF: Future<Output = Result<ModuleResolveResultItem>>,
    {
        Ok(ModuleResolveResult {
            primary: self
                .primary
                .iter()
                .map(|(request, item)| {
                    let asset_fn = &item_fn;
                    let request = request.clone();
                    let item = item.clone();
                    async move { Ok((request, asset_fn(item).await?)) }
                })
                .try_join()
                .await?
                .into_iter()
                .collect(),
            affecting_sources: self.affecting_sources.clone(),
        })
    }

    /// Returns a new [ResolveResult] where all [RequestKey]s are set to the
    /// passed `request`.
    pub fn with_request_ref(&self, request: RcStr) -> Self {
        let new_primary = self
            .primary
            .iter()
            .map(|(k, v)| {
                (
                    RequestKey {
                        request: Some(request.clone()),
                        conditions: k.conditions.clone(),
                    },
                    v.clone(),
                )
            })
            .collect();
        ResolveResult {
            primary: new_primary,
            affecting_sources: self.affecting_sources.clone(),
        }
    }

    pub fn add_conditions<'a>(&mut self, conditions: impl IntoIterator<Item = (&'a str, bool)>) {
        let mut primary = std::mem::take(&mut self.primary);
        for (k, v) in conditions {
            for (key, _) in primary.iter_mut() {
                key.conditions.insert(k.to_string(), v);
            }
        }
        // Deduplicate
        self.primary = IntoIterator::into_iter(primary)
            .collect::<FxIndexMap<_, _>>()
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice();
    }
}

pub struct ResolveResultBuilder {
    pub primary: FxIndexMap<RequestKey, ResolveResultItem>,
    pub affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
}

impl From<ResolveResultBuilder> for ResolveResult {
    fn from(v: ResolveResultBuilder) -> Self {
        ResolveResult {
            primary: v.primary.into_iter().collect(),
            affecting_sources: v.affecting_sources.into_boxed_slice(),
        }
    }
}
impl From<ResolveResult> for ResolveResultBuilder {
    fn from(v: ResolveResult) -> Self {
        ResolveResultBuilder {
            primary: IntoIterator::into_iter(v.primary).collect(),
            affecting_sources: v.affecting_sources.into_vec(),
        }
    }
}
impl ResolveResultBuilder {
    pub fn merge_alternatives(&mut self, other: &ResolveResult) {
        for (k, v) in other.primary.iter() {
            if !self.primary.contains_key(k) {
                self.primary.insert(k.clone(), v.clone());
            }
        }
        let set = self
            .affecting_sources
            .iter()
            .copied()
            .collect::<FxHashSet<_>>();
        self.affecting_sources.extend(
            other
                .affecting_sources
                .iter()
                .filter(|source| !set.contains(source))
                .copied(),
        );
    }
}

#[turbo_tasks::value_impl]
impl ResolveResult {
    #[turbo_tasks::function]
    pub async fn as_raw_module_result(&self) -> Result<Vc<ModuleResolveResult>> {
        Ok(self
            .map_module(|asset| async move {
                Ok(ModuleResolveResultItem::Module(ResolvedVc::upcast(
                    RawModule::new(*asset).to_resolved().await?,
                )))
            })
            .await?
            .cell())
    }

    #[turbo_tasks::function]
    pub async fn with_affecting_source(
        &self,
        source: ResolvedVc<Box<dyn Source>>,
    ) -> Result<Vc<Self>> {
        Ok(Self {
            primary: self.primary.clone(),
            affecting_sources: self
                .affecting_sources
                .iter()
                .copied()
                .chain(std::iter::once(source))
                .collect(),
        }
        .cell())
    }

    #[turbo_tasks::function]
    pub async fn with_affecting_sources(
        &self,
        sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> Result<Vc<Self>> {
        Ok(Self {
            primary: self.primary.clone(),
            affecting_sources: self
                .affecting_sources
                .iter()
                .copied()
                .chain(sources)
                .collect(),
        }
        .cell())
    }

    /// Returns the first [ResolveResult] that is not
    /// [ResolveResult::Unresolvable] in the given list, while keeping track
    /// of all the affecting_sources in all the [ResolveResult]s.
    #[turbo_tasks::function]
    pub async fn select_first(results: Vec<Vc<ResolveResult>>) -> Result<Vc<Self>> {
        let mut affecting_sources = vec![];
        for result in &results {
            affecting_sources.extend(result.await?.get_affecting_sources());
        }
        for result in results {
            let result_ref = result.await?;
            if !result_ref.is_unresolvable_ref() {
                return Ok(Self {
                    primary: result_ref.primary.clone(),
                    affecting_sources: affecting_sources.into_boxed_slice(),
                }
                .cell());
            }
        }
        Ok(*ResolveResult::unresolvable_with_affecting_sources(
            affecting_sources,
        ))
    }

    #[turbo_tasks::function]
    pub async fn alternatives(results: Vec<Vc<ResolveResult>>) -> Result<Vc<Self>> {
        if results.len() == 1 {
            return Ok(results.into_iter().next().unwrap());
        }
        let mut iter = results.into_iter().try_join().await?.into_iter();
        if let Some(current) = iter.next() {
            let mut current: ResolveResultBuilder = ReadRef::into_owned(current).into();
            for result in iter {
                // For clippy -- This explicit deref is necessary
                let other = &*result;
                current.merge_alternatives(other);
            }
            Ok(Self::cell(current.into()))
        } else {
            Ok(*ResolveResult::unresolvable())
        }
    }

    #[turbo_tasks::function]
    pub async fn alternatives_with_affecting_sources(
        results: Vec<Vc<ResolveResult>>,
        affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
    ) -> Result<Vc<Self>> {
        if affecting_sources.is_empty() {
            return Ok(Self::alternatives(results));
        }
        if results.len() == 1 {
            return Ok(results
                .into_iter()
                .next()
                .unwrap()
                .with_affecting_sources(affecting_sources.into_iter().map(|src| *src).collect()));
        }
        let mut iter = results.into_iter().try_join().await?.into_iter();
        if let Some(current) = iter.next() {
            let mut current: ResolveResultBuilder = ReadRef::into_owned(current).into();
            for result in iter {
                // For clippy -- This explicit deref is necessary
                let other = &*result;
                current.merge_alternatives(other);
            }
            current.affecting_sources.extend(affecting_sources);
            Ok(Self::cell(current.into()))
        } else {
            Ok(*ResolveResult::unresolvable_with_affecting_sources(
                affecting_sources,
            ))
        }
    }

    #[turbo_tasks::function]
    pub fn is_unresolvable(&self) -> Vc<bool> {
        Vc::cell(self.is_unresolvable_ref())
    }

    #[turbo_tasks::function]
    pub fn first_source(&self) -> Vc<OptionSource> {
        Vc::cell(self.primary.iter().find_map(|(_, item)| {
            if let &ResolveResultItem::Source(a) = item {
                Some(a)
            } else {
                None
            }
        }))
    }

    #[turbo_tasks::function]
    pub fn primary_sources(&self) -> Vc<Sources> {
        Vc::cell(
            self.primary
                .iter()
                .filter_map(|(_, item)| {
                    if let &ResolveResultItem::Source(a) = item {
                        Some(a)
                    } else {
                        None
                    }
                })
                .collect(),
        )
    }

    /// Returns a new [ResolveResult] where all [RequestKey]s are updated. The `old_request_key`
    /// (prefix) is replaced with the `request_key`. It's not expected that the [ResolveResult]
    /// contains [RequestKey]s that don't have the `old_request_key` prefix, but if there are still
    /// some, they are discarded.
    #[turbo_tasks::function]
    pub fn with_replaced_request_key(
        &self,
        old_request_key: RcStr,
        request_key: RequestKey,
    ) -> Result<Vc<Self>> {
        let new_primary = self
            .primary
            .iter()
            .filter_map(|(k, v)| {
                let remaining = k.request.as_ref()?.strip_prefix(&*old_request_key)?;
                Some((
                    RequestKey {
                        request: request_key
                            .request
                            .as_ref()
                            .map(|r| format!("{r}{remaining}").into()),
                        conditions: request_key.conditions.clone(),
                    },
                    v.clone(),
                ))
            })
            .collect();
        Ok(ResolveResult {
            primary: new_primary,
            affecting_sources: self.affecting_sources.clone(),
        }
        .into())
    }

    /// Returns a new [ResolveResult] where all [RequestKey]s are updated. All keys matching
    /// `old_request_key` are rewritten according to `request_key`. It's not expected that the
    /// [ResolveResult] contains [RequestKey]s that do not match the `old_request_key` prefix, but
    /// if there are still some, they are discarded.
    #[turbo_tasks::function]
    pub async fn with_replaced_request_key_pattern(
        &self,
        old_request_key: Vc<Pattern>,
        request_key: Vc<Pattern>,
    ) -> Result<Vc<Self>> {
        let old_request_key = &*old_request_key.await?;
        let request_key = &*request_key.await?;

        let new_primary = self
            .primary
            .iter()
            .map(|(k, v)| {
                (
                    RequestKey {
                        request: k
                            .request
                            .as_ref()
                            .and_then(|r| old_request_key.match_apply_template(r, request_key))
                            .map(Into::into),
                        conditions: k.conditions.clone(),
                    },
                    v.clone(),
                )
            })
            .collect();
        Ok(ResolveResult {
            primary: new_primary,
            affecting_sources: self.affecting_sources.clone(),
        }
        .into())
    }

    /// Returns a new [ResolveResult] where all [RequestKey]s are set to the
    /// passed `request`.
    #[turbo_tasks::function]
    pub fn with_request(&self, request: RcStr) -> Vc<Self> {
        let new_primary = self
            .primary
            .iter()
            .map(|(k, v)| {
                (
                    RequestKey {
                        request: Some(request.clone()),
                        conditions: k.conditions.clone(),
                    },
                    v.clone(),
                )
            })
            .collect();
        ResolveResult {
            primary: new_primary,
            affecting_sources: self.affecting_sources.clone(),
        }
        .into()
    }
}

#[turbo_tasks::value(transparent)]
pub struct ResolveResultOption(Option<ResolvedVc<ResolveResult>>);

#[turbo_tasks::value_impl]
impl ResolveResultOption {
    #[turbo_tasks::function]
    pub fn some(result: ResolvedVc<ResolveResult>) -> Vc<Self> {
        ResolveResultOption(Some(result)).cell()
    }

    #[turbo_tasks::function]
    pub fn none() -> Vc<Self> {
        ResolveResultOption(None).cell()
    }
}

async fn exists(
    fs_path: Vc<FileSystemPath>,
    refs: &mut Vec<ResolvedVc<Box<dyn Source>>>,
) -> Result<Option<ResolvedVc<FileSystemPath>>> {
    type_exists(fs_path, FileSystemEntryType::File, refs).await
}

async fn dir_exists(
    fs_path: Vc<FileSystemPath>,
    refs: &mut Vec<ResolvedVc<Box<dyn Source>>>,
) -> Result<Option<ResolvedVc<FileSystemPath>>> {
    type_exists(fs_path, FileSystemEntryType::Directory, refs).await
}

async fn type_exists(
    fs_path: Vc<FileSystemPath>,
    ty: FileSystemEntryType,
    refs: &mut Vec<ResolvedVc<Box<dyn Source>>>,
) -> Result<Option<ResolvedVc<FileSystemPath>>> {
    let result = fs_path.resolve().await?.realpath_with_links().await?;
    refs.extend(
        result
            .symlinks
            .iter()
            .map(|path| async move {
                Ok(ResolvedVc::upcast(
                    FileSource::new(**path).to_resolved().await?,
                ))
            })
            .try_join()
            .await?,
    );
    let path = result.path;
    Ok(if *path.get_type().await? == ty {
        Some(path)
    } else {
        None
    })
}

async fn any_exists(
    fs_path: Vc<FileSystemPath>,
    refs: &mut Vec<ResolvedVc<Box<dyn Source>>>,
) -> Result<Option<(FileSystemEntryType, Vc<FileSystemPath>)>> {
    let result = fs_path.resolve().await?.realpath_with_links().await?;
    refs.extend(
        result
            .symlinks
            .iter()
            .map(|path| async move {
                Ok(ResolvedVc::upcast(
                    FileSource::new(**path).to_resolved().await?,
                ))
            })
            .try_join()
            .await?,
    );
    let path = result.path;
    let ty = *path.get_type().await?;
    Ok(
        if matches!(
            ty,
            FileSystemEntryType::NotFound | FileSystemEntryType::Error
        ) {
            None
        } else {
            Some((ty, *path))
        },
    )
}

#[turbo_tasks::value(shared)]
enum ExportsFieldResult {
    Some(#[turbo_tasks(debug_ignore, trace_ignore)] ExportsField),
    None,
}

/// Extracts the "exports" field out of the package.json, parsing it into an
/// appropriate [AliasMap] for lookups.
#[turbo_tasks::function]
async fn exports_field(
    package_json_path: ResolvedVc<FileSystemPath>,
) -> Result<Vc<ExportsFieldResult>> {
    let read = read_package_json(*package_json_path).await?;
    let package_json = match &*read {
        Some(json) => json,
        None => return Ok(ExportsFieldResult::None.cell()),
    };

    let Some(exports) = package_json.get("exports") else {
        return Ok(ExportsFieldResult::None.cell());
    };
    match exports.try_into() {
        Ok(exports) => Ok(ExportsFieldResult::Some(exports).cell()),
        Err(err) => {
            PackageJsonIssue {
                path: package_json_path,
                error_message: err.to_string().into(),
            }
            .resolved_cell()
            .emit();
            Ok(ExportsFieldResult::None.cell())
        }
    }
}

#[turbo_tasks::value(shared)]
enum ImportsFieldResult {
    Some(
        #[turbo_tasks(debug_ignore, trace_ignore)] ImportsField,
        ResolvedVc<FileSystemPath>,
    ),
    None,
}

/// Extracts the "imports" field out of the nearest package.json, parsing it
/// into an appropriate [AliasMap] for lookups.
#[turbo_tasks::function]
async fn imports_field(lookup_path: Vc<FileSystemPath>) -> Result<Vc<ImportsFieldResult>> {
    let package_json_context = find_context_file(lookup_path, package_json()).await?;
    let FindContextFileResult::Found(package_json_path, _refs) = &*package_json_context else {
        return Ok(ImportsFieldResult::None.cell());
    };

    let read = read_package_json(**package_json_path).await?;
    let package_json = match &*read {
        Some(json) => json,
        None => return Ok(ImportsFieldResult::None.cell()),
    };

    let Some(imports) = package_json.get("imports") else {
        return Ok(ImportsFieldResult::None.cell());
    };
    match imports.try_into() {
        Ok(imports) => Ok(ImportsFieldResult::Some(imports, *package_json_path).cell()),
        Err(err) => {
            PackageJsonIssue {
                path: *package_json_path,
                error_message: err.to_string().into(),
            }
            .resolved_cell()
            .emit();
            Ok(ImportsFieldResult::None.cell())
        }
    }
}

#[turbo_tasks::function]
pub fn package_json() -> Vc<Vec<RcStr>> {
    Vc::cell(vec![rcstr!("package.json")])
}

#[turbo_tasks::value(shared)]
pub enum FindContextFileResult {
    Found(ResolvedVc<FileSystemPath>, Vec<ResolvedVc<Box<dyn Source>>>),
    NotFound(Vec<ResolvedVc<Box<dyn Source>>>),
}

#[turbo_tasks::function]
pub async fn find_context_file(
    lookup_path: Vc<FileSystemPath>,
    names: Vc<Vec<RcStr>>,
) -> Result<Vc<FindContextFileResult>> {
    let mut refs = Vec::new();
    for name in &*names.await? {
        let fs_path = lookup_path.join(name.clone());
        if let Some(fs_path) = exists(fs_path, &mut refs).await? {
            return Ok(FindContextFileResult::Found(fs_path, refs).cell());
        }
    }
    if lookup_path.await?.is_root() {
        return Ok(FindContextFileResult::NotFound(refs).cell());
    }
    if refs.is_empty() {
        // Tailcall
        Ok(find_context_file(
            // Hot codepath optimization: resolve all arguments to avoid an automatically-created
            // intermediate task
            lookup_path.parent().resolve().await?,
            names,
        ))
    } else {
        let parent_result = find_context_file(lookup_path.parent().resolve().await?, names).await?;
        Ok(match &*parent_result {
            FindContextFileResult::Found(p, r) => {
                refs.extend(r.iter().copied());
                FindContextFileResult::Found(*p, refs)
            }
            FindContextFileResult::NotFound(r) => {
                refs.extend(r.iter().copied());
                FindContextFileResult::NotFound(refs)
            }
        }
        .cell())
    }
}

// Same as find_context_file, but also stop for package.json with the specified key
#[turbo_tasks::function]
pub async fn find_context_file_or_package_key(
    lookup_path: Vc<FileSystemPath>,
    names: Vc<Vec<RcStr>>,
    package_key: RcStr,
) -> Result<Vc<FindContextFileResult>> {
    let mut refs = Vec::new();
    let package_json_path = lookup_path.join(rcstr!("package.json"));
    if let Some(package_json_path) = exists(package_json_path, &mut refs).await?
        && let Some(json) = &*read_package_json(*package_json_path).await?
        && json.get(&*package_key).is_some()
    {
        return Ok(FindContextFileResult::Found(package_json_path, refs).into());
    }
    for name in &*names.await? {
        let fs_path = lookup_path.join(name.clone());
        if let Some(fs_path) = exists(fs_path, &mut refs).await? {
            return Ok(FindContextFileResult::Found(fs_path, refs).into());
        }
    }
    if lookup_path.await?.is_root() {
        return Ok(FindContextFileResult::NotFound(refs).into());
    }
    if refs.is_empty() {
        // Tailcall
        Ok(find_context_file(
            lookup_path.parent().resolve().await?,
            names,
        ))
    } else {
        let parent_result = find_context_file(lookup_path.parent().resolve().await?, names).await?;
        Ok(match &*parent_result {
            FindContextFileResult::Found(p, r) => {
                refs.extend(r.iter().copied());
                FindContextFileResult::Found(*p, refs)
            }
            FindContextFileResult::NotFound(r) => {
                refs.extend(r.iter().copied());
                FindContextFileResult::NotFound(refs)
            }
        }
        .into())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TraceRawVcs, Debug, NonLocalValue)]
enum FindPackageItem {
    PackageDirectory(ResolvedVc<FileSystemPath>),
    PackageFile(ResolvedVc<FileSystemPath>),
}

#[turbo_tasks::value]
struct FindPackageResult {
    packages: Vec<FindPackageItem>,
    affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
}

#[turbo_tasks::function]
async fn find_package(
    lookup_path: Vc<FileSystemPath>,
    package_name: RcStr,
    options: Vc<ResolveModulesOptions>,
) -> Result<Vc<FindPackageResult>> {
    let mut packages = vec![];
    let mut affecting_sources = vec![];
    let options = options.await?;
    for resolve_modules in &options.modules {
        match resolve_modules {
            ResolveModules::Nested(root_vc, names) => {
                let mut lookup_path = lookup_path;
                let mut lookup_path_value = lookup_path.await?;
                // For clippy -- This explicit deref is necessary
                let root = &*root_vc.await?;
                while lookup_path_value.is_inside_ref(root) {
                    for name in names.iter() {
                        let fs_path = lookup_path.join(name.clone());
                        if let Some(fs_path) = dir_exists(fs_path, &mut affecting_sources).await? {
                            let fs_path = fs_path.join(package_name.clone());
                            if let Some(fs_path) =
                                dir_exists(fs_path, &mut affecting_sources).await?
                            {
                                packages.push(FindPackageItem::PackageDirectory(fs_path));
                            }
                        }
                    }
                    lookup_path = lookup_path.parent().resolve().await?;
                    let new_context_value = lookup_path.await?;
                    if *new_context_value == *lookup_path_value {
                        break;
                    }
                    lookup_path_value = new_context_value;
                }
            }
            ResolveModules::Path {
                dir,
                excluded_extensions,
            } => {
                let excluded_extensions = excluded_extensions.await?;
                let package_dir = dir.join(package_name.clone());
                if let Some((ty, package_dir)) =
                    any_exists(package_dir, &mut affecting_sources).await?
                {
                    match ty {
                        FileSystemEntryType::Directory => {
                            packages.push(FindPackageItem::PackageDirectory(
                                package_dir.to_resolved().await?,
                            ));
                        }
                        FileSystemEntryType::File => {
                            packages.push(FindPackageItem::PackageFile(
                                package_dir.to_resolved().await?,
                            ));
                        }
                        _ => {}
                    }
                }
                for extension in &options.extensions {
                    if excluded_extensions.contains(extension) {
                        continue;
                    }
                    let package_file = package_dir.append(extension.clone());
                    if let Some(package_file) = exists(package_file, &mut affecting_sources).await?
                    {
                        packages.push(FindPackageItem::PackageFile(package_file));
                    }
                }
            }
        }
    }
    Ok(FindPackageResult::cell(FindPackageResult {
        packages,
        affecting_sources,
    }))
}

fn merge_results(results: Vec<Vc<ResolveResult>>) -> Vc<ResolveResult> {
    match results.len() {
        0 => *ResolveResult::unresolvable(),
        1 => results.into_iter().next().unwrap(),
        _ => ResolveResult::alternatives(results),
    }
}

fn merge_results_with_affecting_sources(
    results: Vec<Vc<ResolveResult>>,
    affecting_sources: Vec<ResolvedVc<Box<dyn Source>>>,
) -> Vc<ResolveResult> {
    if affecting_sources.is_empty() {
        return merge_results(results);
    }
    match results.len() {
        0 => *ResolveResult::unresolvable_with_affecting_sources(affecting_sources),
        1 => results
            .into_iter()
            .next()
            .unwrap()
            .with_affecting_sources(affecting_sources.into_iter().map(|src| *src).collect()),
        _ => ResolveResult::alternatives_with_affecting_sources(
            results,
            affecting_sources.into_iter().map(|src| *src).collect(),
        ),
    }
}

#[turbo_tasks::function]
pub async fn resolve_raw(
    lookup_dir: Vc<FileSystemPath>,
    path: Vc<Pattern>,
    force_in_lookup_dir: bool,
) -> Result<Vc<ResolveResult>> {
    async fn to_result(
        request: &str,
        path: ResolvedVc<FileSystemPath>,
    ) -> Result<Vc<ResolveResult>> {
        let RealPathResult { path, symlinks } = &*path.realpath_with_links().await?;
        Ok(*ResolveResult::source_with_affecting_sources(
            RequestKey::new(request.into()),
            ResolvedVc::upcast(FileSource::new(**path).to_resolved().await?),
            symlinks
                .iter()
                .map(|symlink| async move {
                    anyhow::Ok(ResolvedVc::upcast(
                        FileSource::new(**symlink).to_resolved().await?,
                    ))
                })
                .try_join()
                .await?,
        ))
    }

    let mut results = Vec::new();

    let lookup_dir_str = lookup_dir.to_string().await?;
    let pat = path.await?;
    if let Some(pat) = pat
        .filter_could_match("/ROOT/")
        .and_then(|pat| pat.filter_could_not_match("/ROOT/fsd8nz8og54z"))
    {
        let path = Pattern::new(pat);
        let matches = read_matches(lookup_dir.root(), rcstr!("/ROOT/"), true, path).await?;
        if matches.len() > 10000 {
            let path_str = path.to_string().await?;
            println!(
                "WARN: resolving abs pattern {} in {} leads to {} results",
                path_str,
                lookup_dir_str,
                matches.len()
            );
        } else {
            for m in matches.iter() {
                if let PatternMatch::File(request, path) = m {
                    results.push(to_result(request, *path).await?);
                }
            }
        }
    }

    {
        let matches = read_matches(lookup_dir, rcstr!(""), force_in_lookup_dir, path).await?;
        if matches.len() > 10000 {
            println!(
                "WARN: resolving pattern {} in {} leads to {} results",
                pat,
                lookup_dir_str,
                matches.len()
            );
        }
        for m in matches.iter() {
            if let PatternMatch::File(request, path) = m {
                results.push(to_result(request, *path).await?);
            }
        }
    }

    Ok(merge_results(results))
}

#[turbo_tasks::function]
pub async fn resolve(
    lookup_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    resolve_inline(lookup_path, reference_type, request, options).await
}

pub async fn resolve_inline(
    lookup_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    let span = {
        let lookup_path = lookup_path.to_string().await?.to_string();
        let request = request.to_string().await?.to_string();
        tracing::info_span!(
            "resolving",
            lookup_path = lookup_path,
            request = request,
            reference_type = display(&reference_type),
        )
    };
    async {
        let before_plugins_result =
            handle_before_resolve_plugins(lookup_path, reference_type.clone(), request, options)
                .await?;

        let raw_result = match before_plugins_result {
            Some(result) => result,
            None => {
                resolve_internal(lookup_path, request, options)
                    .resolve()
                    .await?
            }
        };

        let result =
            handle_after_resolve_plugins(lookup_path, reference_type, request, options, raw_result)
                .await?;
        Ok(result)
    }
    .instrument(span)
    .await
}

#[turbo_tasks::function]
pub async fn url_resolve(
    origin: Vc<Box<dyn ResolveOrigin>>,
    request: Vc<Request>,
    reference_type: ReferenceType,
    issue_source: Option<IssueSource>,
    is_optional: bool,
) -> Result<Vc<ModuleResolveResult>> {
    let resolve_options = origin.resolve_options(reference_type.clone());
    let rel_request = request.as_relative();
    let rel_result = resolve(
        origin.origin_path().parent(),
        reference_type.clone(),
        rel_request,
        resolve_options,
    );
    let result = if *rel_result.is_unresolvable().await? && rel_request.resolve().await? != request
    {
        resolve(
            origin.origin_path().parent(),
            reference_type.clone(),
            request,
            resolve_options,
        )
        .with_affecting_sources(
            rel_result
                .await?
                .get_affecting_sources()
                .map(|src| *src)
                .collect(),
        )
    } else {
        rel_result
    };
    let result = origin
        .asset_context()
        .process_resolve_result(result, reference_type.clone());
    handle_resolve_error(
        result,
        reference_type,
        origin.origin_path(),
        request,
        resolve_options,
        is_optional,
        issue_source,
    )
    .await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn handle_before_resolve_plugins(
    lookup_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
) -> Result<Option<Vc<ResolveResult>>> {
    for plugin in &options.await?.before_resolve_plugins {
        let condition = plugin.before_resolve_condition().resolve().await?;
        if !*condition.matches(request).await? {
            continue;
        }

        if let Some(result) = *plugin
            .before_resolve(lookup_path, reference_type.clone(), request)
            .await?
        {
            return Ok(Some(*result));
        }
    }
    Ok(None)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn handle_after_resolve_plugins(
    lookup_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
    result: Vc<ResolveResult>,
) -> Result<Vc<ResolveResult>> {
    async fn apply_plugins_to_path(
        path: Vc<FileSystemPath>,
        lookup_path: Vc<FileSystemPath>,
        reference_type: ReferenceType,
        request: Vc<Request>,
        options: Vc<ResolveOptions>,
    ) -> Result<Option<Vc<ResolveResult>>> {
        for plugin in &options.await?.after_resolve_plugins {
            let after_resolve_condition = plugin.after_resolve_condition().resolve().await?;
            if *after_resolve_condition.matches(path).await?
                && let Some(result) = *plugin
                    .after_resolve(path, lookup_path, reference_type.clone(), request)
                    .await?
            {
                return Ok(Some(*result));
            }
        }
        Ok(None)
    }

    let mut changed = false;
    let result_value = result.await?;

    let mut new_primary = FxIndexMap::default();
    let mut new_affecting_sources = Vec::new();

    for (key, primary) in result_value.primary.iter() {
        if let &ResolveResultItem::Source(source) = primary {
            let path = source.ident().path().resolve().await?;
            if let Some(new_result) =
                apply_plugins_to_path(path, lookup_path, reference_type.clone(), request, options)
                    .await?
            {
                let new_result = new_result.await?;
                changed = true;
                new_primary.extend(
                    new_result
                        .primary
                        .iter()
                        .map(|(_, item)| (key.clone(), item.clone())),
                );
                new_affecting_sources.extend(new_result.affecting_sources.iter().copied());
            } else {
                new_primary.insert(key.clone(), primary.clone());
            }
        } else {
            new_primary.insert(key.clone(), primary.clone());
        }
    }

    if !changed {
        return Ok(result);
    }

    let mut affecting_sources = result_value.affecting_sources.to_vec();
    affecting_sources.append(&mut new_affecting_sources);

    Ok(ResolveResult {
        primary: new_primary.into_iter().collect(),
        affecting_sources: affecting_sources.into_boxed_slice(),
    }
    .cell())
}

#[turbo_tasks::function]
async fn resolve_internal(
    lookup_path: ResolvedVc<FileSystemPath>,
    request: ResolvedVc<Request>,
    options: ResolvedVc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    resolve_internal_inline(*lookup_path, *request, *options).await
}

async fn resolve_internal_inline(
    lookup_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    let span = {
        let lookup_path = lookup_path.to_string().await?.to_string();
        let request = request.to_string().await?.to_string();
        tracing::info_span!(
            "internal resolving",
            lookup_path = lookup_path,
            request = request
        )
    };
    async move {
        let options_value: &ResolveOptions = &*options.await?;

        let request_value = request.await?;

        // Apply import mappings if provided
        let mut has_alias = false;
        if let Some(import_map) = &options_value.import_map {
            let request_parts = match &*request_value {
                Request::Alternatives { requests } => requests.as_slice(),
                _ => &[request.to_resolved().await?],
            };
            for &request in request_parts {
                let result = import_map.await?.lookup(lookup_path, *request).await?;
                if !matches!(result, ImportMapResult::NoEntry) {
                    has_alias = true;
                    let resolved_result = resolve_import_map_result(
                        &result,
                        lookup_path,
                        lookup_path,
                        *request,
                        options,
                        request.query().owned().await?,
                    )
                    .await?;
                    // We might have matched an alias in the import map, but there is no guarantee
                    // the alias actually resolves to something. For instance, a tsconfig.json
                    // `compilerOptions.paths` option might alias "@*" to "./*", which
                    // would also match a request to "@emotion/core". Here, we follow what the
                    // Typescript resolution algorithm does in case an alias match
                    // doesn't resolve to anything: fall back to resolving the request normally.
                    if let Some(result) = resolved_result
                        && !*result.is_unresolvable().await?
                    {
                        return Ok(result);
                    }
                }
            }
        }

        let result = match &*request_value {
            Request::Dynamic => *ResolveResult::unresolvable(),
            Request::Alternatives { requests } => {
                let results = requests
                    .iter()
                    .map(|req| async { resolve_internal_inline(lookup_path, **req, options).await })
                    .try_join()
                    .await?;

                merge_results(results)
            }
            Request::Raw {
                path,
                query,
                force_in_lookup_dir,
                fragment,
            } => {
                let mut results = Vec::new();
                let matches = read_matches(
                    lookup_path,
                    rcstr!(""),
                    *force_in_lookup_dir,
                    Pattern::new(path.clone()).resolve().await?,
                )
                .await?;

                for m in matches.iter() {
                    match m {
                        PatternMatch::File(matched_pattern, path) => {
                            results.push(
                                resolved(
                                    RequestKey::new(matched_pattern.clone()),
                                    **path,
                                    lookup_path,
                                    request,
                                    options_value,
                                    options,
                                    query.clone(),
                                    fragment.clone(),
                                )
                                .await?,
                            );
                        }
                        PatternMatch::Directory(matched_pattern, path) => {
                            results.push(
                                resolve_into_folder(**path, options)
                                    .with_request(matched_pattern.clone()),
                            );
                        }
                    }
                }

                merge_results(results)
            }
            Request::Relative {
                path,
                query,
                force_in_lookup_dir,
                fragment,
            } => {
                if !fragment.is_empty()
                    && let Ok(result) = resolve_relative_request(
                        lookup_path,
                        request,
                        options,
                        options_value,
                        path,
                        query.clone(),
                        *force_in_lookup_dir,
                        fragment.clone(),
                    )
                    .await
                {
                    return Ok(result);
                }
                // Resolve without fragment
                resolve_relative_request(
                    lookup_path,
                    request,
                    options,
                    options_value,
                    path,
                    query.clone(),
                    *force_in_lookup_dir,
                    RcStr::default(),
                )
                .await?
            }
            Request::Module {
                module,
                path,
                query,
                fragment,
            } => {
                resolve_module_request(
                    lookup_path,
                    request,
                    options,
                    options_value,
                    module,
                    path,
                    query.clone(),
                    fragment.clone(),
                )
                .await?
            }
            Request::ServerRelative {
                path,
                query,
                fragment,
            } => {
                let mut new_pat = path.clone();
                new_pat.push_front(rcstr!(".").into());
                let relative = Request::relative(new_pat, query.clone(), fragment.clone(), true);

                if !has_alias {
                    ResolvingIssue {
                        severity: error_severity(options).await?,
                        request_type: "server relative import: not implemented yet".to_string(),
                        request: relative.to_resolved().await?,
                        file_path: lookup_path.to_resolved().await?,
                        resolve_options: options.to_resolved().await?,
                        error_message: Some(
                            "server relative imports are not implemented yet. Please try an \
                             import relative to the file you are importing from."
                                .to_string(),
                        ),
                        source: None,
                    }
                    .resolved_cell()
                    .emit();
                }

                Box::pin(resolve_internal_inline(
                    lookup_path.root(),
                    relative,
                    options,
                ))
                .await?
            }
            Request::Windows {
                path: _,
                query: _,
                fragment: _,
            } => {
                if !has_alias {
                    ResolvingIssue {
                        severity: error_severity(options).await?,
                        request_type: "windows import: not implemented yet".to_string(),
                        request: request.to_resolved().await?,
                        file_path: lookup_path.to_resolved().await?,
                        resolve_options: options.to_resolved().await?,
                        error_message: Some("windows imports are not implemented yet".to_string()),
                        source: None,
                    }
                    .resolved_cell()
                    .emit();
                }

                *ResolveResult::unresolvable()
            }
            Request::Empty => *ResolveResult::unresolvable(),
            Request::PackageInternal { path } => {
                let options_value = options.await?;
                let (conditions, unspecified_conditions) = options_value
                    .in_package
                    .iter()
                    .find_map(|item| match item {
                        ResolveInPackage::ImportsField {
                            conditions,
                            unspecified_conditions,
                        } => Some((Cow::Borrowed(conditions), *unspecified_conditions)),
                        _ => None,
                    })
                    .unwrap_or_else(|| (Default::default(), ConditionValue::Unset));
                resolve_package_internal_with_imports_field(
                    lookup_path,
                    request,
                    options,
                    path,
                    &conditions,
                    &unspecified_conditions,
                )
                .await?
            }
            Request::DataUri {
                media_type,
                encoding,
                data,
            } => {
                // Behave like Request::Uri
                let uri: RcStr = stringify_data_uri(media_type, encoding, *data)
                    .await?
                    .into();
                if options.await?.parse_data_uris {
                    *ResolveResult::primary_with_key(
                        RequestKey::new(uri.clone()),
                        ResolveResultItem::Source(ResolvedVc::upcast(
                            DataUriSource::new(
                                media_type.clone(),
                                encoding.clone(),
                                **data,
                                lookup_path,
                            )
                            .to_resolved()
                            .await?,
                        )),
                    )
                } else {
                    *ResolveResult::primary_with_key(
                        RequestKey::new(uri.clone()),
                        ResolveResultItem::External {
                            name: uri,
                            ty: ExternalType::Url,
                            traced: ExternalTraced::Untraced,
                        },
                    )
                }
            }
            Request::Uri {
                protocol,
                remainder,
                query: _,
                fragment: _,
            } => {
                let uri: RcStr = format!("{protocol}{remainder}").into();
                *ResolveResult::primary_with_key(
                    RequestKey::new(uri.clone()),
                    ResolveResultItem::External {
                        name: uri,
                        ty: ExternalType::Url,
                        traced: ExternalTraced::Untraced,
                    },
                )
            }
            Request::Unknown { path } => {
                if !has_alias {
                    ResolvingIssue {
                        severity: error_severity(options).await?,
                        request_type: format!("unknown import: `{path}`"),
                        request: request.to_resolved().await?,
                        file_path: lookup_path.to_resolved().await?,
                        resolve_options: options.to_resolved().await?,
                        error_message: None,
                        source: None,
                    }
                    .resolved_cell()
                    .emit();
                }
                *ResolveResult::unresolvable()
            }
        };

        // Apply fallback import mappings if provided
        if let Some(import_map) = &options_value.fallback_import_map
            && *result.is_unresolvable().await?
        {
            let result = import_map.await?.lookup(lookup_path, request).await?;
            let resolved_result = resolve_import_map_result(
                &result,
                lookup_path,
                lookup_path,
                request,
                options,
                request.query().owned().await?,
            )
            .await?;
            if let Some(result) = resolved_result
                && !*result.is_unresolvable().await?
            {
                return Ok(result);
            }
        }

        Ok(result)
    }
    .instrument(span)
    .await
}

#[turbo_tasks::function]
async fn resolve_into_folder(
    package_path: ResolvedVc<FileSystemPath>,
    options: Vc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    let package_json_path = package_path.join(rcstr!("package.json"));
    let options_value = options.await?;

    for resolve_into_package in options_value.into_package.iter() {
        match resolve_into_package {
            ResolveIntoPackage::MainField { field: name } => {
                if let Some(package_json) = &*read_package_json(package_json_path).await?
                    && let Some(field_value) = package_json[name.as_str()].as_str()
                {
                    let normalized_request: RcStr = normalize_request(field_value).into();
                    if normalized_request.is_empty()
                        || &*normalized_request == "."
                        || &*normalized_request == "./"
                    {
                        continue;
                    }
                    let request = Request::parse_string(normalized_request);

                    // main field will always resolve not fully specified
                    let options = if options_value.fully_specified {
                        options.with_fully_specified(false).resolve().await?
                    } else {
                        options
                    };
                    let result = &*resolve_internal_inline(*package_path, request, options)
                        .await?
                        .await?;
                    // we are not that strict when a main field fails to resolve
                    // we continue to try other alternatives
                    if !result.is_unresolvable_ref() {
                        let mut result: ResolveResultBuilder =
                            result.with_request_ref(rcstr!(".")).into();
                        result.affecting_sources.push(ResolvedVc::upcast(
                            FileSource::new(package_json_path).to_resolved().await?,
                        ));
                        return Ok(ResolveResult::from(result).cell());
                    }
                };
            }
            ResolveIntoPackage::ExportsField { .. } => {}
        }
    }

    if options_value.fully_specified {
        return Ok(*ResolveResult::unresolvable());
    }

    // fall back to dir/index.[js,ts,...]
    let pattern = match &options_value.default_files[..] {
        [] => return Ok(*ResolveResult::unresolvable()),
        [file] => Pattern::Constant(format!("./{file}").into()),
        files => Pattern::Alternatives(
            files
                .iter()
                .map(|file| Pattern::Constant(format!("./{file}").into()))
                .collect(),
        ),
    };

    let request = Request::parse(pattern);

    Ok(resolve_internal_inline(*package_path, request, options)
        .await?
        .with_request(rcstr!(".")))
}

#[tracing::instrument(level = Level::TRACE, skip_all)]
async fn resolve_relative_request(
    lookup_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
    options_value: &ResolveOptions,
    path_pattern: &Pattern,
    query: RcStr,
    force_in_lookup_dir: bool,
    fragment: RcStr,
) -> Result<Vc<ResolveResult>> {
    // Check alias field for aliases first
    let lookup_path_ref = &*lookup_path.await?;
    if let Some(result) = apply_in_package(
        lookup_path,
        options,
        options_value,
        |package_path| {
            let request = path_pattern.as_string()?;
            let prefix_path = package_path.get_path_to(lookup_path_ref)?;
            let request = normalize_request(&format!("./{prefix_path}/{request}"));
            Some(request.into())
        },
        query.clone(),
        fragment.clone(),
    )
    .await?
    {
        return Ok(result);
    }

    let mut new_path = path_pattern.clone();

    if !fragment.is_empty() {
        new_path.push(Pattern::Alternatives(vec![
            Pattern::Constant(RcStr::default()),
            Pattern::Constant(fragment.clone()),
        ]));
    }

    if !options_value.fully_specified {
        // Add the extensions as alternatives to the path
        // read_matches keeps the order of alternatives intact
        new_path.push(Pattern::Alternatives(
            once(Pattern::Constant(RcStr::default()))
                .chain(
                    options_value
                        .extensions
                        .iter()
                        .map(|ext| Pattern::Constant(ext.clone())),
                )
                .collect(),
        ));
        new_path.normalize();
    };

    if options_value.enable_typescript_with_output_extension {
        new_path.replace_final_constants(&|c: &RcStr| -> Option<Pattern> {
            let (base, replacement) = match c.rsplit_once(".") {
                Some((base, "js")) => (
                    base,
                    vec![
                        Pattern::Constant(rcstr!(".ts")),
                        Pattern::Constant(rcstr!(".tsx")),
                        Pattern::Constant(rcstr!(".js")),
                    ],
                ),
                Some((base, "mjs")) => (
                    base,
                    vec![
                        Pattern::Constant(rcstr!(".mts")),
                        Pattern::Constant(rcstr!(".mjs")),
                    ],
                ),
                Some((base, "cjs")) => (
                    base,
                    vec![
                        Pattern::Constant(rcstr!(".cts")),
                        Pattern::Constant(rcstr!(".cjs")),
                    ],
                ),
                _ => {
                    return None;
                }
            };
            if base.is_empty() {
                Some(Pattern::Alternatives(replacement))
            } else {
                Some(Pattern::Concatenation(vec![
                    Pattern::Constant(base.into()),
                    Pattern::Alternatives(replacement),
                ]))
            }
        });
        new_path.normalize();
    }

    let mut results = Vec::new();
    let matches = read_matches(
        lookup_path,
        rcstr!(""),
        force_in_lookup_dir,
        Pattern::new(new_path).resolve().await?,
    )
    .await?;

    for m in matches.iter() {
        if let PatternMatch::File(matched_pattern, path) = m {
            let mut pushed = false;
            if !options_value.fully_specified {
                for ext in options_value.extensions.iter() {
                    let Some(matched_pattern) = matched_pattern.strip_suffix(&**ext) else {
                        continue;
                    };

                    if !fragment.is_empty() {
                        // If the fragment is not empty, we need to strip it from the matched
                        // pattern
                        if let Some(matched_pattern) = matched_pattern
                            .strip_suffix(fragment.as_str())
                            .and_then(|s| s.strip_suffix('#'))
                        {
                            results.push(
                                resolved(
                                    RequestKey::new(matched_pattern.into()),
                                    **path,
                                    lookup_path,
                                    request,
                                    options_value,
                                    options,
                                    query.clone(),
                                    RcStr::default(),
                                )
                                .await?,
                            );
                            pushed = true;
                        }
                    }
                    if !pushed && path_pattern.is_match(matched_pattern) {
                        results.push(
                            resolved(
                                RequestKey::new(matched_pattern.into()),
                                **path,
                                lookup_path,
                                request,
                                options_value,
                                options,
                                query.clone(),
                                fragment.clone(),
                            )
                            .await?,
                        );
                        pushed = true;
                    }
                }
            }
            if !fragment.is_empty() {
                // If the fragment is not empty, we need to strip it from the matched pattern
                if let Some(matched_pattern) = matched_pattern.strip_suffix(fragment.as_str()) {
                    results.push(
                        resolved(
                            RequestKey::new(matched_pattern.into()),
                            **path,
                            lookup_path,
                            request,
                            options_value,
                            options,
                            query.clone(),
                            RcStr::default(),
                        )
                        .await?,
                    );
                    pushed = true;
                }
            }

            if !pushed || path_pattern.is_match(matched_pattern) {
                results.push(
                    resolved(
                        RequestKey::new(matched_pattern.clone()),
                        **path,
                        lookup_path,
                        request,
                        options_value,
                        options,
                        query.clone(),
                        fragment.clone(),
                    )
                    .await?,
                );
            }
        }
    }
    // Directory matches must be resolved AFTER file matches
    for m in matches.iter() {
        if let PatternMatch::Directory(matched_pattern, path) = m {
            results
                .push(resolve_into_folder(**path, options).with_request(matched_pattern.clone()));
        }
    }

    Ok(merge_results(results))
}

#[tracing::instrument(level = Level::TRACE, skip_all)]
async fn apply_in_package(
    lookup_path: Vc<FileSystemPath>,
    options: Vc<ResolveOptions>,
    options_value: &ResolveOptions,
    get_request: impl Fn(&FileSystemPath) -> Option<RcStr>,
    query: RcStr,
    fragment: RcStr,
) -> Result<Option<Vc<ResolveResult>>> {
    // Check alias field for module aliases first
    for in_package in options_value.in_package.iter() {
        // resolve_module_request is called when importing a node
        // module, not a PackageInternal one, so the imports field
        // doesn't apply.
        let ResolveInPackage::AliasField(field) = in_package else {
            continue;
        };

        let FindContextFileResult::Found(package_json_path, refs) =
            &*find_context_file(lookup_path, package_json().resolve().await?).await?
        else {
            continue;
        };

        let read = read_package_json(**package_json_path).await?;
        let Some(package_json) = &*read else {
            continue;
        };

        let Some(field_value) = package_json[field.as_str()].as_object() else {
            continue;
        };

        let package_path = package_json_path.parent().resolve().await?;

        let Some(request) = get_request(&*package_path.await?) else {
            continue;
        };

        let value = if let Some(value) = field_value.get(&*request) {
            value
        } else if let Some(request) = request.strip_prefix("./") {
            let Some(value) = field_value.get(request) else {
                continue;
            };
            value
        } else {
            continue;
        };

        let refs = refs.clone();
        let request_key = RequestKey::new(request.clone());

        if value.as_bool() == Some(false) {
            return Ok(Some(*ResolveResult::primary_with_affecting_sources(
                request_key,
                ResolveResultItem::Ignore,
                refs,
            )));
        }

        if let Some(value) = value.as_str() {
            if value == &*request {
                // This would be a cycle, so we ignore it
                return Ok(None);
            }
            return Ok(Some(
                resolve_internal(
                    package_path,
                    Request::parse(Pattern::Constant(value.into()))
                        .with_query(query.clone())
                        .with_fragment(fragment.clone()),
                    options,
                )
                .with_replaced_request_key(value.into(), request_key)
                .with_affecting_sources(refs.into_iter().map(|src| *src).collect()),
            ));
        }

        ResolvingIssue {
            severity: error_severity(options).await?,
            file_path: *package_json_path,
            request_type: format!("alias field ({field})"),
            request: Request::parse(Pattern::Constant(request))
                .to_resolved()
                .await?,
            resolve_options: options.to_resolved().await?,
            error_message: Some(format!("invalid alias field value: {value}")),
            source: None,
        }
        .resolved_cell()
        .emit();

        return Ok(Some(*ResolveResult::unresolvable_with_affecting_sources(
            refs,
        )));
    }
    Ok(None)
}

#[turbo_tasks::value]
enum FindSelfReferencePackageResult {
    Found {
        name: String,
        package_path: ResolvedVc<FileSystemPath>,
    },
    NotFound,
}

#[turbo_tasks::function]
/// Finds the nearest folder containing package.json that could be used for a
/// self-reference (i.e. has an exports fields).
async fn find_self_reference(
    lookup_path: Vc<FileSystemPath>,
) -> Result<Vc<FindSelfReferencePackageResult>> {
    let package_json_context = find_context_file(lookup_path, package_json()).await?;
    if let FindContextFileResult::Found(package_json_path, _refs) = &*package_json_context {
        let read = read_package_json(**package_json_path).await?;
        if let Some(json) = &*read
            && json.get("exports").is_some()
            && let Some(name) = json["name"].as_str()
        {
            return Ok(FindSelfReferencePackageResult::Found {
                name: name.to_string(),
                package_path: package_json_path.parent().to_resolved().await?,
            }
            .cell());
        }
    }
    Ok(FindSelfReferencePackageResult::NotFound.cell())
}

#[tracing::instrument(level = Level::TRACE, skip_all)]
async fn resolve_module_request(
    lookup_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    options: Vc<ResolveOptions>,
    options_value: &ResolveOptions,
    module: &str,
    path: &Pattern,
    query: RcStr,
    fragment: RcStr,
) -> Result<Vc<ResolveResult>> {
    // Check alias field for module aliases first
    if let Some(result) = apply_in_package(
        lookup_path,
        options,
        options_value,
        |_| {
            let full_pattern = Pattern::concat([RcStr::from(module).into(), path.clone()]);
            full_pattern.into_string()
        },
        query.clone(),
        fragment.clone(),
    )
    .await?
    {
        return Ok(result);
    }

    // Self references, if the nearest package.json has the name of the requested
    // module. This should match only using the exports field and no other
    // fields/fallbacks.
    if let FindSelfReferencePackageResult::Found { name, package_path } =
        &*find_self_reference(lookup_path).await?
        && name == module
    {
        let result = resolve_into_package(
            path.clone(),
            **package_path,
            query.clone(),
            fragment.clone(),
            options,
        );
        if !(*result.is_unresolvable().await?) {
            return Ok(result);
        }
    }

    let result = find_package(
        lookup_path,
        module.into(),
        resolve_modules_options(options).resolve().await?,
    )
    .await?;

    if result.packages.is_empty() {
        return Ok(*ResolveResult::unresolvable_with_affecting_sources(
            result.affecting_sources.clone(),
        ));
    }

    let mut results = vec![];

    // There may be more than one package with the same name. For instance, in a
    // TypeScript project, `compilerOptions.baseUrl` can declare a path where to
    // resolve packages. A request to "foo/bar" might resolve to either
    // "[baseUrl]/foo/bar" or "[baseUrl]/node_modules/foo/bar", and we'll need to
    // try both.
    for item in &result.packages {
        match *item {
            FindPackageItem::PackageDirectory(package_path) => {
                results.push(resolve_into_package(
                    path.clone(),
                    *package_path,
                    query.clone(),
                    fragment.clone(),
                    options,
                ));
            }
            FindPackageItem::PackageFile(package_path) => {
                if path.is_match("") {
                    let resolved = resolved(
                        RequestKey::new(rcstr!(".")),
                        *package_path,
                        lookup_path,
                        request,
                        options_value,
                        options,
                        query.clone(),
                        fragment.clone(),
                    )
                    .await?;
                    results.push(resolved)
                }
            }
        }
    }

    let module_result =
        merge_results_with_affecting_sources(results, result.affecting_sources.clone())
            .with_replaced_request_key(rcstr!("."), RequestKey::new(module.into()));

    if options_value.prefer_relative {
        let module_prefix: RcStr = format!("./{module}").into();
        let pattern = Pattern::concat([
            module_prefix.clone().into(),
            rcstr!("/").into(),
            path.clone(),
        ]);
        let relative = Request::relative(pattern, query, fragment, true)
            .to_resolved()
            .await?;
        let relative_result =
            Box::pin(resolve_internal_inline(lookup_path, *relative, options)).await?;
        let relative_result = relative_result
            .with_replaced_request_key(module_prefix, RequestKey::new(module.into()));

        Ok(merge_results(vec![relative_result, module_result]))
    } else {
        Ok(module_result)
    }
}

#[turbo_tasks::function]
async fn resolve_into_package(
    path: Pattern,
    package_path: ResolvedVc<FileSystemPath>,
    query: RcStr,
    fragment: RcStr,
    options: ResolvedVc<ResolveOptions>,
) -> Result<Vc<ResolveResult>> {
    let options_value = options.await?;
    let mut results = Vec::new();

    let is_root_match = path.is_match("") || path.is_match("/");
    let could_match_others = path.could_match_others("");

    for resolve_into_package in options_value.into_package.iter() {
        match resolve_into_package {
            // handled by the `resolve_into_folder` call below
            ResolveIntoPackage::MainField { .. } => {}
            ResolveIntoPackage::ExportsField {
                conditions,
                unspecified_conditions,
            } => {
                let package_json_path = package_path.join(rcstr!("package.json"));
                let ExportsFieldResult::Some(exports_field) =
                    &*exports_field(package_json_path).await?
                else {
                    continue;
                };

                let Some(path) = path.clone().into_string() else {
                    todo!("pattern into an exports field is not implemented yet");
                };

                let path = if &*path == "/" {
                    ".".to_string()
                } else {
                    format!(".{path}")
                };

                results.push(
                    handle_exports_imports_field(
                        *package_path,
                        package_json_path,
                        *options,
                        exports_field,
                        &path,
                        conditions,
                        unspecified_conditions,
                        query,
                    )
                    .await?,
                );

                // other options do not apply anymore when an exports
                // field exist
                return Ok(merge_results(results));
            }
        }
    }

    // apply main field(s) or fallback to index.js if there's no subpath
    if is_root_match {
        results.push(resolve_into_folder(
            *package_path,
            options.with_fully_specified(false),
        ));
    }

    if could_match_others {
        let mut new_pat = path.clone();
        new_pat.push_front(rcstr!(".").into());

        let relative = Request::relative(new_pat, query, fragment, true)
            .to_resolved()
            .await?;
        results.push(resolve_internal_inline(*package_path, *relative, *options).await?);
    }

    Ok(merge_results(results))
}

#[tracing::instrument(level = Level::TRACE, skip_all)]
async fn resolve_import_map_result(
    result: &ImportMapResult,
    lookup_path: Vc<FileSystemPath>,
    original_lookup_path: Vc<FileSystemPath>,
    original_request: Vc<Request>,
    options: Vc<ResolveOptions>,
    query: RcStr,
) -> Result<Option<Vc<ResolveResult>>> {
    Ok(match result {
        ImportMapResult::Result(result) => Some(**result),
        ImportMapResult::Alias(request, alias_lookup_path) => {
            let request = **request;
            let lookup_path = match alias_lookup_path {
                Some(path) => **path,
                None => lookup_path,
            };
            // We must avoid cycles during resolving
            if request == original_request && lookup_path == original_lookup_path {
                None
            } else {
                let result = resolve_internal(lookup_path, request, options);
                Some(result.with_replaced_request_key_pattern(
                    request.request_pattern(),
                    original_request.request_pattern(),
                ))
            }
        }
        ImportMapResult::External(name, ty, traced) => {
            Some(*ResolveResult::primary(ResolveResultItem::External {
                name: name.clone(),
                ty: *ty,
                traced: *traced,
            }))
        }
        ImportMapResult::AliasExternal {
            name,
            ty,
            traced,
            lookup_dir: alias_lookup_path,
        } => {
            let request = Request::parse_string(name.clone());

            // We must avoid cycles during resolving
            if request.resolve().await? == original_request
                && **alias_lookup_path == original_lookup_path
            {
                None
            } else {
                let is_external_resolvable = !resolve_internal(
                    **alias_lookup_path,
                    request,
                    match ty {
                        ExternalType::Url => options,
                        // TODO is that root correct?
                        ExternalType::CommonJs => {
                            node_cjs_resolve_options(alias_lookup_path.root())
                        }
                        ExternalType::EcmaScriptModule => {
                            node_esm_resolve_options(alias_lookup_path.root())
                        }
                        ExternalType::Global => options,
                    },
                )
                .await?
                .is_unresolvable_ref();
                if is_external_resolvable {
                    Some(*ResolveResult::primary(ResolveResultItem::External {
                        name: name.clone(),
                        ty: *ty,
                        traced: *traced,
                    }))
                } else {
                    None
                }
            }
        }
        ImportMapResult::Alternatives(list) => {
            let results = list
                .iter()
                .map(|result| {
                    Box::pin(resolve_import_map_result(
                        result,
                        lookup_path,
                        original_lookup_path,
                        original_request,
                        options,
                        query.clone(),
                    ))
                })
                .try_join()
                .await?;

            Some(merge_results(results.into_iter().flatten().collect()))
        }
        ImportMapResult::NoEntry => None,
    })
}

#[tracing::instrument(level = Level::TRACE, skip_all)]
async fn resolved(
    request_key: RequestKey,
    fs_path: Vc<FileSystemPath>,
    original_context: Vc<FileSystemPath>,
    original_request: Vc<Request>,
    options_value: &ResolveOptions,
    options: Vc<ResolveOptions>,
    query: RcStr,
    fragment: RcStr,
) -> Result<Vc<ResolveResult>> {
    let RealPathResult { path, symlinks } = &*fs_path.realpath_with_links().await?;

    let path_ref = &*path.await?;
    // Check alias field for path aliases first
    if let Some(result) = apply_in_package(
        path.parent().resolve().await?,
        options,
        options_value,
        |package_path| package_path.get_relative_path_to(path_ref),
        query.clone(),
        fragment.clone(),
    )
    .await?
    {
        return Ok(result);
    }

    if let Some(resolved_map) = options_value.resolved_map {
        let result = resolved_map
            .lookup(**path, original_context, original_request)
            .await?;

        let resolved_result = resolve_import_map_result(
            &result,
            path.parent(),
            original_context,
            original_request,
            options,
            query.clone(),
        )
        .await?;

        if let Some(result) = resolved_result {
            return Ok(result);
        }
    }

    Ok(*ResolveResult::source_with_affecting_sources(
        request_key,
        ResolvedVc::upcast(
            FileSource::new_with_query_and_fragment(**path, query, fragment)
                .to_resolved()
                .await?,
        ),
        symlinks
            .iter()
            .map(|symlink| async move {
                anyhow::Ok(ResolvedVc::upcast(
                    FileSource::new(**symlink).to_resolved().await?,
                ))
            })
            .try_join()
            .await?,
    ))
}

async fn handle_exports_imports_field(
    package_path: Vc<FileSystemPath>,
    package_json_path: Vc<FileSystemPath>,
    options: Vc<ResolveOptions>,
    exports_imports_field: &AliasMap<SubpathValue>,
    path: &str,
    conditions: &BTreeMap<RcStr, ConditionValue>,
    unspecified_conditions: &ConditionValue,
    query: RcStr,
) -> Result<Vc<ResolveResult>> {
    let mut results = Vec::new();
    let mut conditions_state = FxHashMap::default();

    let req = Pattern::Constant(format!("{path}{query}").into());

    let values = exports_imports_field
        .lookup(&req)
        .map(AliasMatch::try_into_self)
        .collect::<Result<Vec<_>>>()?;

    for value in values.iter() {
        if value.add_results(
            conditions,
            unspecified_conditions,
            &mut conditions_state,
            &mut results,
        ) {
            break;
        }
    }

    let mut resolved_results = Vec::new();
    for (result_path, conditions) in results {
        if let Some(result_path) = result_path.with_normalized_path() {
            let request = Request::parse(Pattern::Concatenation(vec![
                Pattern::Constant(rcstr!("./")),
                result_path,
            ]))
            .to_resolved()
            .await?;

            let resolve_result =
                Box::pin(resolve_internal_inline(package_path, *request, options)).await?;
            if conditions.is_empty() {
                resolved_results.push(resolve_result.with_request(path.into()));
            } else {
                let mut resolve_result = resolve_result.await?.with_request_ref(path.into());
                resolve_result.add_conditions(conditions);
                resolved_results.push(resolve_result.cell());
            }
        }
    }

    // other options do not apply anymore when an exports field exist
    Ok(merge_results_with_affecting_sources(
        resolved_results,
        vec![ResolvedVc::upcast(
            FileSource::new(package_json_path).to_resolved().await?,
        )],
    ))
}

/// Resolves a `#dep` import using the containing package.json's `imports`
/// field. The dep may be a constant string or a pattern, and the values can be
/// static strings or conditions like `import` or `require` to handle ESM/CJS
/// with differently compiled files.
async fn resolve_package_internal_with_imports_field(
    file_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    resolve_options: Vc<ResolveOptions>,
    pattern: &Pattern,
    conditions: &BTreeMap<RcStr, ConditionValue>,
    unspecified_conditions: &ConditionValue,
) -> Result<Vc<ResolveResult>> {
    let Pattern::Constant(specifier) = pattern else {
        bail!("PackageInternal requests can only be Constant strings");
    };
    // https://github.com/nodejs/node/blob/1b177932/lib/internal/modules/esm/resolve.js#L615-L619
    if specifier == "#" || specifier.starts_with("#/") || specifier.ends_with('/') {
        ResolvingIssue {
            severity: error_severity(resolve_options).await?,
            file_path: file_path.to_resolved().await?,
            request_type: format!("package imports request: `{specifier}`"),
            request: request.to_resolved().await?,
            resolve_options: resolve_options.to_resolved().await?,
            error_message: None,
            source: None,
        }
        .resolved_cell()
        .emit();
        return Ok(*ResolveResult::unresolvable());
    }

    let imports_result = imports_field(file_path).await?;
    let (imports, package_json_path) = match &*imports_result {
        ImportsFieldResult::Some(i, p) => (i, *p),
        ImportsFieldResult::None => return Ok(*ResolveResult::unresolvable()),
    };

    handle_exports_imports_field(
        package_json_path.parent(),
        *package_json_path,
        resolve_options,
        imports,
        specifier,
        conditions,
        unspecified_conditions,
        RcStr::default(),
    )
    .await
}

pub async fn handle_resolve_error(
    result: Vc<ModuleResolveResult>,
    reference_type: ReferenceType,
    origin_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    resolve_options: Vc<ResolveOptions>,
    is_optional: bool,
    source: Option<IssueSource>,
) -> Result<Vc<ModuleResolveResult>> {
    async fn is_unresolvable(result: Vc<ModuleResolveResult>) -> Result<bool> {
        Ok(*result.resolve().await?.is_unresolvable().await?)
    }
    Ok(match is_unresolvable(result).await {
        Ok(unresolvable) => {
            if unresolvable {
                emit_unresolvable_issue(
                    is_optional,
                    origin_path,
                    reference_type,
                    request,
                    resolve_options,
                    source,
                )
                .await?;
            }

            result
        }
        Err(err) => {
            emit_resolve_error_issue(
                is_optional,
                origin_path,
                reference_type,
                request,
                resolve_options,
                err,
                source,
            )
            .await?;
            *ModuleResolveResult::unresolvable()
        }
    })
}

pub async fn handle_resolve_source_error(
    result: Vc<ResolveResult>,
    reference_type: ReferenceType,
    origin_path: Vc<FileSystemPath>,
    request: Vc<Request>,
    resolve_options: Vc<ResolveOptions>,
    is_optional: bool,
    source: Option<IssueSource>,
) -> Result<Vc<ResolveResult>> {
    async fn is_unresolvable(result: Vc<ResolveResult>) -> Result<bool> {
        Ok(*result.resolve().await?.is_unresolvable().await?)
    }
    Ok(match is_unresolvable(result).await {
        Ok(unresolvable) => {
            if unresolvable {
                emit_unresolvable_issue(
                    is_optional,
                    origin_path,
                    reference_type,
                    request,
                    resolve_options,
                    source,
                )
                .await?;
            }

            result
        }
        Err(err) => {
            emit_resolve_error_issue(
                is_optional,
                origin_path,
                reference_type,
                request,
                resolve_options,
                err,
                source,
            )
            .await?;
            *ResolveResult::unresolvable()
        }
    })
}

async fn emit_resolve_error_issue(
    is_optional: bool,
    origin_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    resolve_options: Vc<ResolveOptions>,
    err: anyhow::Error,
    source: Option<IssueSource>,
) -> Result<()> {
    let severity = if is_optional || resolve_options.await?.loose_errors {
        IssueSeverity::Warning
    } else {
        IssueSeverity::Error
    };
    ResolvingIssue {
        severity,
        file_path: origin_path.to_resolved().await?,
        request_type: format!("{reference_type} request"),
        request: request.to_resolved().await?,
        resolve_options: resolve_options.to_resolved().await?,
        error_message: Some(format!("{}", PrettyPrintError(&err))),
        source,
    }
    .resolved_cell()
    .emit();
    Ok(())
}

async fn emit_unresolvable_issue(
    is_optional: bool,
    origin_path: Vc<FileSystemPath>,
    reference_type: ReferenceType,
    request: Vc<Request>,
    resolve_options: Vc<ResolveOptions>,
    source: Option<IssueSource>,
) -> Result<()> {
    let severity = if is_optional || resolve_options.await?.loose_errors {
        IssueSeverity::Warning
    } else {
        IssueSeverity::Error
    };
    ResolvingIssue {
        severity,
        file_path: origin_path.to_resolved().await?,
        request_type: format!("{reference_type} request"),
        request: request.to_resolved().await?,
        resolve_options: resolve_options.to_resolved().await?,
        error_message: None,
        source,
    }
    .resolved_cell()
    .emit();
    Ok(())
}

async fn error_severity(resolve_options: Vc<ResolveOptions>) -> Result<IssueSeverity> {
    Ok(if resolve_options.await?.loose_errors {
        IssueSeverity::Warning
    } else {
        IssueSeverity::Error
    })
}

/// ModulePart represents a part of a module.
///
/// Currently this is used only for ESMs.
#[derive(
    Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, TraceRawVcs, TaskInput, NonLocalValue,
)]
pub enum ModulePart {
    /// Represents the side effects of a module. This part is evaluated even if
    /// all exports are unused.
    Evaluation,
    /// Represents an export of a module.
    Export(RcStr),
    /// Represents a renamed export of a module.
    RenamedExport {
        original_export: RcStr,
        export: RcStr,
    },
    /// Represents a namespace object of a module exported as named export.
    RenamedNamespace { export: RcStr },
    /// A pointer to a specific part.
    Internal(u32),
    /// The local declarations of a module.
    Locals,
    /// The whole exports of a module.
    Exports,
    /// A facade of the module behaving like the original, but referencing
    /// internal parts.
    Facade,
}

impl ModulePart {
    pub fn evaluation() -> Self {
        ModulePart::Evaluation
    }

    pub fn export(export: RcStr) -> Self {
        ModulePart::Export(export)
    }

    pub fn renamed_export(original_export: RcStr, export: RcStr) -> Self {
        ModulePart::RenamedExport {
            original_export,
            export,
        }
    }

    pub fn renamed_namespace(export: RcStr) -> Self {
        ModulePart::RenamedNamespace { export }
    }

    pub fn internal(id: u32) -> Self {
        ModulePart::Internal(id)
    }

    pub fn locals() -> Self {
        ModulePart::Locals
    }

    pub fn exports() -> Self {
        ModulePart::Exports
    }

    pub fn facade() -> Self {
        ModulePart::Facade
    }
}

impl Display for ModulePart {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ModulePart::Evaluation => f.write_str("module evaluation"),
            ModulePart::Export(export) => write!(f, "export {export}"),
            ModulePart::RenamedExport {
                original_export,
                export,
            } => write!(f, "export {original_export} as {export}"),
            ModulePart::RenamedNamespace { export } => {
                write!(f, "export * as {export}")
            }
            ModulePart::Internal(id) => write!(f, "internal part {id}"),
            ModulePart::Locals => f.write_str("locals"),
            ModulePart::Exports => f.write_str("exports"),
            ModulePart::Facade => f.write_str("facade"),
        }
    }
}
