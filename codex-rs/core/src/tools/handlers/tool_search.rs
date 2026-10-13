use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::ToolSearchSourceListing;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use bm25::Embedder;
use bm25::EmbedderBuilder;
use bm25::Language;
use bm25::Scorer;
use codex_prompts::ResolvedModelMessages;
use codex_tools::IndirectNamespacePrefixes;
use codex_tools::LoadableToolSpec;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchEntry;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use tracing::instrument;

pub struct ToolSearchHandler {
    search_infos: Vec<ToolSearchInfo>,
    source_listing: ToolSearchSourceListing,
    spec: ToolSpec,
    search_embedder: Embedder,
    search_scorer: Scorer<usize>,
}

#[derive(Default)]
pub(crate) struct ToolSearchHandlerCache {
    cached: Mutex<Option<CachedToolSearchHandler>>,
}

struct CachedToolSearchHandler {
    handler: Arc<ToolSearchHandler>,
    sources: Vec<ToolSearchSource>,
}

enum ToolSearchSource {
    Immutable(Weak<dyn CoreToolRuntime>),
    Dynamic(Box<ToolSearchInfo>),
}

impl ToolSearchHandlerCache {
    #[instrument(level = "trace", skip_all)]
    pub(crate) fn get_or_build(
        &self,
        registry: &ToolRegistry,
        source_listing: ToolSearchSourceListing,
    ) -> Arc<ToolSearchHandler> {
        let sources = registry
            .entries()
            .filter(|tool| tool.exposure.is_deferred())
            .filter_map(|tool| {
                if let Some(runtime) = tool.cached_runtime() {
                    Some(ToolSearchSource::Immutable(Arc::downgrade(runtime)))
                } else {
                    registry
                        .search_info(tool)
                        .map(Box::new)
                        .map(ToolSearchSource::Dynamic)
                }
            })
            .collect::<Vec<_>>();

        {
            let cached = self.cached();
            if let Some(cached) = cached.as_ref()
                && cached.handler.source_listing == source_listing
                && Self::sources_match(&cached.sources, &sources)
            {
                return Arc::clone(&cached.handler);
            }
        }

        let search_infos = sources
            .iter()
            .filter_map(|source| match source {
                ToolSearchSource::Immutable(runtime) => {
                    runtime.upgrade().and_then(|runtime| runtime.search_info())
                }
                ToolSearchSource::Dynamic(search_info) => Some(search_info.as_ref().clone()),
            })
            .collect();

        let handler = Arc::new(ToolSearchHandler::new(search_infos, source_listing));
        let mut cached = self.cached();
        if let Some(cached) = cached.as_ref()
            && cached.handler.source_listing == source_listing
            && Self::sources_match(&cached.sources, &sources)
        {
            return Arc::clone(&cached.handler);
        }
        *cached = Some(CachedToolSearchHandler {
            handler: Arc::clone(&handler),
            sources,
        });
        handler
    }

    fn sources_match(cached_sources: &[ToolSearchSource], sources: &[ToolSearchSource]) -> bool {
        cached_sources.len() == sources.len()
            && cached_sources
                .iter()
                .zip(sources)
                .all(|(cached, current)| match (cached, current) {
                    (ToolSearchSource::Immutable(cached), ToolSearchSource::Immutable(current)) => {
                        Weak::ptr_eq(cached, current)
                    }
                    (ToolSearchSource::Dynamic(cached), ToolSearchSource::Dynamic(current)) => {
                        cached == current
                    }
                    (ToolSearchSource::Immutable(_), ToolSearchSource::Dynamic(_))
                    | (ToolSearchSource::Dynamic(_), ToolSearchSource::Immutable(_)) => false,
                })
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, Option<CachedToolSearchHandler>> {
        match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl ToolSearchHandler {
    #[instrument(
        level = "trace",
        skip_all,
        fields(search_info_count = search_infos.len())
    )]
    pub(crate) fn new(
        search_infos: Vec<ToolSearchInfo>,
        source_listing: ToolSearchSourceListing,
    ) -> Self {
        let search_source_infos = search_infos
            .iter()
            .filter_map(|search_info| search_info.source_info.clone())
            .collect::<Vec<_>>();
        let spec = create_tool_search_tool(
            &search_source_infos,
            TOOL_SEARCH_DEFAULT_LIMIT,
            source_listing,
        );
        let corpus = search_infos
            .iter()
            .map(|search_info| search_info.entry.search_text.as_str())
            .collect::<Vec<_>>();
        let search_embedder =
            EmbedderBuilder::with_fit_to_corpus(Language::English, &corpus).build();
        let mut search_scorer = Scorer::new();
        for (id, text) in corpus.into_iter().enumerate() {
            search_scorer.upsert(&id, search_embedder.embed(text));
        }

        Self {
            search_infos,
            source_listing,
            spec,
            search_embedder,
            search_scorer,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ToolSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ToolSearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            payload,
            step_context,
            ..
        } = invocation;

        let args = match payload {
            ToolPayload::ToolSearch { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "query must not be empty".to_string(),
            ));
        }
        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);

        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        let model_messages = ResolvedModelMessages::from_model(&step_context.settings.model_info);
        let indirect_prefixes = IndirectNamespacePrefixes::new(
            model_messages.indirect_description_prefixes(),
            step_context.tool_router.mcp_namespaces(),
        )
        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?;

        if self.search_infos.is_empty() {
            return Ok(boxed_tool_output(ToolSearchOutput { tools: Vec::new() }));
        }

        let mut tools = self.search(query, limit);
        indirect_prefixes.apply_search(&mut tools);

        Ok(boxed_tool_output(ToolSearchOutput { tools }))
    }
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search(&self, query: &str, limit: usize) -> Vec<LoadableToolSpec> {
        coalesce_loadable_tool_specs(
            self.search_entries(query)
                .take(limit)
                .map(ToolSearchEntry::to_loadable_spec),
        )
    }

    fn search_entries(&self, query: &str) -> impl Iterator<Item = &ToolSearchEntry> + '_ {
        self.search_scorer
            .matches(&self.search_embedder.embed(query))
            .into_iter()
            .filter_map(|result| self.search_infos.get(result.id))
            .map(|search_info| &search_info.entry)
    }
}
