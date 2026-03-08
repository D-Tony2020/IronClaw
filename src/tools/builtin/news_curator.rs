//! Native news curator tool.
//!
//! Fetches tech news from curated RSS/Atom feeds and Hacker News API,
//! deduplicates by title, filters by topic keywords, and returns structured JSON.
//!
//! This replaces the WASM news_curator tool with a native implementation:
//! - Parallel HTTP fetching via `tokio::JoinSet` (vs serial WASM host calls)
//! - Proper memory management (Rust allocator vs WASM linear memory)
//! - Direct reqwest access (no host function boundary overhead)
//! - `feed-rs` not required: lightweight manual XML parsing (same as WASM version)

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, require_str};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const USER_AGENT: &str = concat!(
    "IronClaw-NewsCurator/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/nearai/ironclaw)"
);
const DEFAULT_MAX_ARTICLES: usize = 50;
const HN_TOP_STORIES_URL: &str = "https://hacker-news.firebaseio.com/v0/topstories.json";
const HN_ITEM_URL: &str = "https://hacker-news.firebaseio.com/v0/item";
const MAX_HN_STORIES: usize = 30;
const MAX_SUMMARY_LEN: usize = 500;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Max response body for a single feed (2 MB).
const MAX_FEED_SIZE: usize = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Feed definitions
// ---------------------------------------------------------------------------

struct FeedDef {
    name: &'static str,
    url: &'static str,
    priority: &'static str,
    #[allow(dead_code)]
    lang: &'static str,
}

const FEEDS: &[FeedDef] = &[
    // === Core Tier (critical) ===
    FeedDef { name: "OpenAI Blog", url: "https://openai.com/news/rss.xml", priority: "critical", lang: "en" },
    FeedDef { name: "Anthropic Research", url: "https://raw.githubusercontent.com/Olshansk/rss-feeds/main/feeds/feed_anthropic_research.xml", priority: "critical", lang: "en" },
    FeedDef { name: "Google DeepMind", url: "https://deepmind.google/blog/rss.xml", priority: "critical", lang: "en" },
    FeedDef { name: "Google Research", url: "https://research.google/blog/rss", priority: "critical", lang: "en" },
    FeedDef { name: "Meta AI / FAIR", url: "https://research.facebook.com/feed", priority: "critical", lang: "en" },
    FeedDef { name: "Qwen 通义千问", url: "https://qwenlm.github.io/feed.xml", priority: "critical", lang: "zh" },
    FeedDef { name: "机器之心", url: "https://www.jiqizhixin.com/rss", priority: "critical", lang: "zh" },
    // === Core Tier (high) ===
    FeedDef { name: "TechCrunch AI", url: "https://techcrunch.com/category/artificial-intelligence/feed/", priority: "high", lang: "en" },
    FeedDef { name: "The Verge AI", url: "https://www.theverge.com/rss/ai-artificial-intelligence/index.xml", priority: "high", lang: "en" },
    FeedDef { name: "Ars Technica AI", url: "https://arstechnica.com/ai/feed", priority: "high", lang: "en" },
    FeedDef { name: "MIT Technology Review", url: "https://www.technologyreview.com/topic/artificial-intelligence/feed", priority: "high", lang: "en" },
    FeedDef { name: "量子位", url: "https://www.qbitai.com/feed", priority: "high", lang: "zh" },
    FeedDef { name: "36氪", url: "https://36kr.com/feed", priority: "high", lang: "zh" },
    // === Extended Tier ===
    FeedDef { name: "IEEE Spectrum Robotics", url: "https://spectrum.ieee.org/feeds/topic/robotics.rss", priority: "medium", lang: "en" },
    FeedDef { name: "The Robot Report", url: "https://www.therobotreport.com/feed/", priority: "medium", lang: "en" },
    FeedDef { name: "NVIDIA Developer Blog", url: "https://developer.nvidia.com/blog/feed", priority: "medium", lang: "en" },
    FeedDef { name: "Apple Newsroom", url: "https://www.apple.com/newsroom/rss-feed.rss", priority: "medium", lang: "en" },
    FeedDef { name: "IEEE Semiconductors", url: "https://spectrum.ieee.org/feeds/topic/semiconductors.rss", priority: "medium", lang: "en" },
    FeedDef { name: "Tom's Hardware", url: "https://www.tomshardware.com/feeds/all", priority: "medium", lang: "en" },
    FeedDef { name: "HF Daily Papers", url: "https://papers.takara.ai/api/feed", priority: "medium", lang: "en" },
    FeedDef { name: "HF Trending Models", url: "https://zernel.github.io/huggingface-trending-feed/feed.xml", priority: "medium", lang: "en" },
    FeedDef { name: "Import AI", url: "https://importai.substack.com/feed", priority: "medium", lang: "en" },
    FeedDef { name: "ChinAI Newsletter", url: "https://chinai.substack.com/feed", priority: "medium", lang: "en" },
    // === Community Tier ===
    FeedDef { name: "Hacker News 100+", url: "https://hnrss.org/newest?points=100", priority: "medium", lang: "en" },
    FeedDef { name: "Reddit ML+LocalLLaMA", url: "https://www.reddit.com/r/MachineLearning+LocalLLaMA/.rss", priority: "medium", lang: "en" },
    FeedDef { name: "GitHub Trending Python", url: "https://mshibanami.github.io/GitHubTrendingRSS/daily/python.xml", priority: "low", lang: "en" },
    FeedDef { name: "TechCrunch Funding", url: "https://techcrunch.com/tag/funding/feed", priority: "medium", lang: "en" },
    FeedDef { name: "Hugging Face Blog", url: "https://huggingface.co/blog/feed.xml", priority: "medium", lang: "en" },
];

// ---------------------------------------------------------------------------
// Topic filter keywords
// ---------------------------------------------------------------------------

const TOPIC_KEYWORDS: &[&str] = &[
    // Primary
    "ai", "llm", "大语言模型", "large language model", "foundation model",
    "multimodal", "reasoning model", "agent", "rag",
    // Entities
    "anthropic", "openai", "deepmind", "deepseek", "mistral", "xai",
    "meta ai", "qwen", "通义千问", "figure ai", "unitree", "宇树",
    "1x technologies", "boston dynamics", "tesla bot", "optimus",
    "nvidia", "amd", "tsmc", "apple", "intel",
    // Products
    "claude", "gpt", "gemini", "grok", "llama", "mixtral",
    // Embodied AI
    "具身智能", "embodied intelligence", "humanoid robot", "人形机器人",
    "robotic manipulation", "diffusion policy", "locomotion", "quadruped",
    // Hardware
    "智能硬件", "smart hardware", "chip", "semiconductor", "半导体",
    "gpu", "tpu", "edge ai", "ar/vr", "spatial computing",
    // Business
    "funding", "融资", "收购", "acquisition", "ipo", "ai startup",
    "valuation", "series a", "series b", "series c",
    // Trends
    "scaling law", "rlhf", "dpo", "moe", "mixture of experts",
    "synthetic data", "alignment", "safety", "open source", "open weight",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Article {
    title: String,
    link: String,
    summary: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub_date: Option<String>,
    priority: String,
    categories: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HnStory {
    title: Option<String>,
    url: Option<String>,
    score: Option<u32>,
    time: Option<u64>,
    descendants: Option<u32>,
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

/// Native news curator tool.
///
/// Replaces the WASM news_curator with parallel HTTP fetching and proper
/// memory management. No sandbox overhead for read-only HTTP GET operations.
pub struct NewsCuratorTool {
    client: Arc<Client>,
}

impl NewsCuratorTool {
    /// Create a new news curator tool.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client: Arc::new(client),
        }
    }
}

impl Default for NewsCuratorTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for NewsCuratorTool {
    fn name(&self) -> &str {
        "news_curator"
    }

    fn description(&self) -> &str {
        "Fetch, deduplicate, and filter tech news from curated RSS feeds and \
         Hacker News. Returns a JSON array of articles with title, link, \
         summary, source, priority, and categories. Use the 'fetch' action \
         to collect articles, or 'sources' to list configured feeds."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: 'fetch' to collect and filter news articles, 'sources' to list configured feeds",
                    "enum": ["fetch", "sources"]
                },
                "max_articles": {
                    "type": "integer",
                    "description": "Maximum number of articles to return (default 50)",
                    "minimum": 1,
                    "maximum": 200
                },
                "lookback_hours": {
                    "type": "integer",
                    "description": "How many hours back to fetch (default 48)",
                    "minimum": 1,
                    "maximum": 168
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let action = require_str(&params, "action")?;

        match action {
            "fetch" => {
                let max_articles = params
                    .get("max_articles")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(DEFAULT_MAX_ARTICLES);

                let result = self.action_fetch(max_articles).await?;
                Ok(ToolOutput::success(result, start.elapsed()))
            }
            "sources" => {
                let result = action_sources();
                Ok(ToolOutput::success(result, start.elapsed()))
            }
            _ => Err(ToolError::InvalidParameters(format!(
                "Unknown action '{}'. Valid actions: fetch, sources",
                action
            ))),
        }
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(120) // RSS fetching can be slow
    }

    fn requires_sanitization(&self) -> bool {
        true // External data
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never // Read-only GET requests to public feeds
    }

    fn estimated_duration(&self, _params: &serde_json::Value) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }

    fn rate_limit_config(&self) -> Option<crate::tools::tool::ToolRateLimitConfig> {
        Some(crate::tools::tool::ToolRateLimitConfig::new(5, 30))
    }
}

impl NewsCuratorTool {
    /// Fetch articles from all feeds in parallel, deduplicate, filter, and sort.
    async fn action_fetch(&self, max_articles: usize) -> Result<serde_json::Value, ToolError> {
        tracing::info!("news_curator: starting parallel fetch of {} feeds + HN", FEEDS.len());

        // Spawn parallel feed fetches
        let mut join_set = JoinSet::new();

        for (idx, feed) in FEEDS.iter().enumerate() {
            let client = Arc::clone(&self.client);
            let url = feed.url.to_string();
            let name = feed.name.to_string();
            let priority = feed.priority.to_string();

            join_set.spawn(async move {
                let result = fetch_rss_feed(&client, &url, &name, &priority).await;
                (idx, name, result)
            });
        }

        // Spawn HN fetch
        let hn_client = Arc::clone(&self.client);
        join_set.spawn(async move {
            let result = fetch_hacker_news(&hn_client).await;
            (usize::MAX, "Hacker News API".to_string(), result)
        });

        // Collect results
        let mut all_articles: Vec<Article> = Vec::new();
        let mut feeds_ok = 0usize;
        let mut feeds_err = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((_idx, name, Ok(articles))) => {
                    tracing::debug!(feed = %name, count = articles.len(), "Feed fetched");
                    feeds_ok += 1;
                    all_articles.extend(articles);
                }
                Ok((_idx, name, Err(e))) => {
                    tracing::warn!(feed = %name, error = %e, "Feed fetch failed");
                    feeds_err += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Feed task panicked");
                    feeds_err += 1;
                }
            }
        }

        let total_raw = all_articles.len();
        tracing::info!("news_curator: raw={}, feeds_ok={}, feeds_err={}", total_raw, feeds_ok, feeds_err);

        // Deduplicate by normalized title
        let before_dedup = all_articles.len();
        all_articles = dedup_articles(all_articles);
        tracing::debug!(
            "Dedup: {} → {} (removed {})",
            before_dedup,
            all_articles.len(),
            before_dedup - all_articles.len()
        );

        // Filter by topic keywords
        let before_filter = all_articles.len();
        all_articles = filter_by_topic(all_articles);
        tracing::debug!(
            "Filter: {} → {} (removed {} off-topic)",
            before_filter,
            all_articles.len(),
            before_filter - all_articles.len()
        );

        // Sort by priority
        sort_articles(&mut all_articles);

        // Truncate
        all_articles.truncate(max_articles);

        Ok(serde_json::json!({
            "article_count": all_articles.len(),
            "total_raw": total_raw,
            "feeds_ok": feeds_ok,
            "feeds_error": feeds_err,
            "articles": all_articles,
        }))
    }
}

// ---------------------------------------------------------------------------
// Sources action
// ---------------------------------------------------------------------------

fn action_sources() -> serde_json::Value {
    let sources: Vec<serde_json::Value> = FEEDS
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "url": f.url,
                "priority": f.priority,
                "lang": f.lang,
            })
        })
        .collect();

    serde_json::json!({
        "feed_count": FEEDS.len(),
        "feeds": sources,
        "hn_enabled": true,
        "topic_keyword_count": TOPIC_KEYWORDS.len(),
    })
}

// ---------------------------------------------------------------------------
// RSS/Atom feed fetching (async, parallel)
// ---------------------------------------------------------------------------

async fn fetch_rss_feed(
    client: &Client,
    url: &str,
    feed_name: &str,
    priority: &str,
) -> Result<Vec<Article>, String> {
    let resp = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "application/rss+xml, application/atom+xml, application/xml, text/xml, */*",
        )
        .send()
        .await
        .map_err(|e| format!("HTTP error for '{}': {}", feed_name, e))?;

    let status = resp.status().as_u16();
    if status < 200 || status >= 300 {
        return Err(format!("HTTP {} for '{}'", status, feed_name));
    }

    // Read body with size limit
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Read body error for '{}': {}", feed_name, e))?;

    if body.len() > MAX_FEED_SIZE {
        return Err(format!(
            "Feed '{}' too large: {} bytes (max {})",
            feed_name,
            body.len(),
            MAX_FEED_SIZE
        ));
    }

    parse_feed_xml(&body, feed_name, priority)
}

/// Parse RSS 2.0 or Atom XML into articles.
fn parse_feed_xml(xml: &str, feed_name: &str, priority: &str) -> Result<Vec<Article>, String> {
    let mut articles = Vec::new();

    // Determine format: Atom uses <entry>, RSS uses <item>
    let is_atom = xml.contains("<feed") && xml.contains("<entry");
    let tag = if is_atom { "entry" } else { "item" };

    let items = extract_elements(xml, tag);

    for item_xml in items {
        let title = extract_text(&item_xml, "title").unwrap_or_default();
        if title.is_empty() {
            continue;
        }

        let link = if is_atom {
            extract_atom_link(&item_xml).or_else(|| extract_text(&item_xml, "link"))
        } else {
            extract_text(&item_xml, "link")
        }
        .unwrap_or_default();

        if link.is_empty() {
            continue;
        }

        let summary = extract_text(&item_xml, "summary")
            .or_else(|| extract_text(&item_xml, "description"))
            .or_else(|| extract_text(&item_xml, "content"))
            .or_else(|| extract_text(&item_xml, "content:encoded"))
            .map(|s| clean_html(&s))
            .unwrap_or_default();

        let summary = truncate_str(&summary, MAX_SUMMARY_LEN);

        let pub_date = extract_text(&item_xml, "pubDate")
            .or_else(|| extract_text(&item_xml, "published"))
            .or_else(|| extract_text(&item_xml, "updated"))
            .or_else(|| extract_text(&item_xml, "dc:date"));

        let categories = extract_all_text(&item_xml, "category");

        articles.push(Article {
            title,
            link,
            summary,
            source: feed_name.to_string(),
            pub_date,
            priority: priority.to_string(),
            categories,
        });
    }

    Ok(articles)
}

// ---------------------------------------------------------------------------
// Hacker News fetch
// ---------------------------------------------------------------------------

async fn fetch_hacker_news(client: &Client) -> Result<Vec<Article>, String> {
    // 1. Get top story IDs
    let resp = client
        .get(HN_TOP_STORIES_URL)
        .send()
        .await
        .map_err(|e| format!("HN top stories: {}", e))?;

    if resp.status().as_u16() != 200 {
        return Err(format!("HN HTTP {}", resp.status().as_u16()));
    }

    let ids: Vec<u64> = resp
        .json()
        .await
        .map_err(|e| format!("HN parse IDs: {}", e))?;

    // 2. Fetch individual stories in parallel (batched)
    let mut join_set = JoinSet::new();

    for &id in ids.iter().take(MAX_HN_STORIES) {
        let client = client.clone();
        join_set.spawn(async move {
            let url = format!("{}/{}.json", HN_ITEM_URL, id);
            let result = client
                .get(&url)
                .timeout(Duration::from_secs(10))
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    resp.json::<HnStory>().await.ok().and_then(|story| {
                        let title = story.title?;
                        let link = story
                            .url
                            .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={}", id));
                        let score = story.score.unwrap_or(0);
                        let priority = if score > 200 { "high" } else { "medium" };
                        let summary = format!(
                            "HN score: {} | comments: {}",
                            score,
                            story.descendants.unwrap_or(0)
                        );
                        let pub_date = story.time.map(format_unix_timestamp);

                        Some(Article {
                            title,
                            link,
                            summary,
                            source: "Hacker News".to_string(),
                            pub_date,
                            priority: priority.to_string(),
                            categories: vec!["tech".to_string()],
                        })
                    })
                }
                _ => None,
            }
        });
    }

    let mut articles = Vec::new();
    while let Some(result) = join_set.join_next().await {
        if let Ok(Some(article)) = result {
            articles.push(article);
        }
    }

    Ok(articles)
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

fn dedup_articles(articles: Vec<Article>) -> Vec<Article> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut result: Vec<Article> = Vec::new();

    for article in articles {
        let key = normalize_title(&article.title);
        if let Some(&idx) = seen.get(&key) {
            // Keep higher priority version
            if priority_rank(&article.priority) < priority_rank(&result[idx].priority) {
                result[idx] = article;
            }
        } else {
            seen.insert(key, result.len());
            result.push(article);
        }
    }

    result
}

fn normalize_title(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c > '\u{4e00}')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// Topic filtering
// ---------------------------------------------------------------------------

fn filter_by_topic(articles: Vec<Article>) -> Vec<Article> {
    let core_sources: HashSet<&str> = [
        "OpenAI Blog",
        "Anthropic Research",
        "Google DeepMind",
        "Google Research",
        "Meta AI / FAIR",
        "Qwen 通义千问",
        "机器之心",
    ]
    .into_iter()
    .collect();

    articles
        .into_iter()
        .filter(|a| {
            if core_sources.contains(a.source.as_str()) {
                return true;
            }
            let haystack = format!("{} {} {}", a.title, a.summary, a.categories.join(" "))
                .to_lowercase();

            TOPIC_KEYWORDS.iter().any(|kw| haystack.contains(kw))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

fn sort_articles(articles: &mut [Article]) {
    articles.sort_by(|a, b| {
        let pa = priority_rank(&a.priority);
        let pb = priority_rank(&b.priority);
        pa.cmp(&pb)
    });
}

// ---------------------------------------------------------------------------
// XML helpers (lightweight, no external lib)
// ---------------------------------------------------------------------------

/// Extract all occurrences of `<tag>...</tag>` from XML.
fn extract_elements(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut results = Vec::new();
    let mut search_from = 0;

    while let Some(start) = xml[search_from..].find(&open) {
        let abs_start = search_from + start;
        if let Some(end_of_close) = xml[abs_start..].find(&close) {
            let abs_end = abs_start + end_of_close + close.len();
            results.push(xml[abs_start..abs_end].to_string());
            search_from = abs_end;
        } else {
            break;
        }
    }

    results
}

/// Extract text content of the first `<tag>...</tag>`.
fn extract_text(xml: &str, tag: &str) -> Option<String> {
    let open_start = format!("<{}", tag);
    let close = format!("</{}>", tag);

    let start_pos = xml.find(&open_start)?;
    let after_open = xml[start_pos..].find('>')? + start_pos + 1;

    let content_start = if xml[after_open..].starts_with("<![CDATA[") {
        after_open + 9
    } else {
        after_open
    };

    let end_pos = xml[content_start..].find(&close)? + content_start;

    let mut content = &xml[content_start..end_pos];

    if let Some(stripped) = content.strip_suffix("]]>") {
        content = stripped;
    }

    let text = content.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(decode_xml_entities(&text))
    }
}

/// Extract all text values of `<tag>` elements.
fn extract_all_text(xml: &str, tag: &str) -> Vec<String> {
    let mut results = Vec::new();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut pos = 0;

    while let Some(start) = xml[pos..].find(&open) {
        let abs_start = pos + start;
        if let Some(gt) = xml[abs_start..].find('>') {
            let content_start = abs_start + gt + 1;
            if let Some(end) = xml[content_start..].find(&close) {
                let text = xml[content_start..content_start + end].trim();
                if !text.is_empty() {
                    results.push(decode_xml_entities(text));
                }
                pos = content_start + end + close.len();
            } else {
                // Self-closing or attribute-only — check for term= attribute
                let tag_end = abs_start + gt;
                let tag_str = &xml[abs_start..=tag_end];
                if tag_str.contains("term=\"") {
                    if let Some(t) = extract_attr(tag_str, "term") {
                        results.push(t);
                    }
                }
                pos = tag_end + 1;
            }
        } else {
            break;
        }
    }

    results
}

/// Extract Atom `<link href="..."/>`.
fn extract_atom_link(xml: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut pos = 0;

    while let Some(start) = xml[pos..].find("<link") {
        let abs_start = pos + start;
        let tag_end = xml[abs_start..].find('>')? + abs_start;
        let tag = &xml[abs_start..=tag_end];

        let href = extract_attr(tag, "href");
        let rel = extract_attr(tag, "rel");

        if let Some(href) = href {
            match rel.as_deref() {
                Some("alternate") | None => return Some(href),
                _ => {
                    if best.is_none() {
                        best = Some(href);
                    }
                }
            }
        }
        pos = tag_end + 1;
    }

    best
}

/// Extract attribute value from an XML tag string.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(decode_xml_entities(&tag[start..end]))
}

/// Strip HTML tags and decode entities.
fn clean_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    let decoded = decode_xml_entities(&result);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&#8217;", "\u{2019}")
        .replace("&#8220;", "\u{201C}")
        .replace("&#8221;", "\u{201D}")
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &s[..boundary])
    }
}

/// Format a Unix timestamp into an ISO 8601 date string.
fn format_unix_timestamp(ts: u64) -> String {
    let secs = ts;
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let mins = (remaining % 3600) / 60;

    let (year, month, day) = days_to_date(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:00Z")
}

fn days_to_date(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let months: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut month = 1u64;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_title() {
        assert_eq!(
            normalize_title("  OpenAI releases GPT-5!  "),
            "openai releases gpt5"
        );
        assert_eq!(
            normalize_title("DeepSeek 发布新模型"),
            "deepseek 发布新模型"
        );
    }

    #[test]
    fn test_priority_rank() {
        assert!(priority_rank("critical") < priority_rank("high"));
        assert!(priority_rank("high") < priority_rank("medium"));
        assert!(priority_rank("medium") < priority_rank("low"));
    }

    #[test]
    fn test_clean_html() {
        assert_eq!(clean_html("<p>Hello <b>world</b></p>"), "Hello world");
        assert_eq!(
            clean_html("No &amp; tags &lt;here&gt;"),
            "No & tags <here>"
        );
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello...");
    }

    #[test]
    fn test_extract_text() {
        let xml =
            r#"<item><title>Test Article</title><link>https://example.com</link></item>"#;
        assert_eq!(
            extract_text(xml, "title"),
            Some("Test Article".to_string())
        );
        assert_eq!(
            extract_text(xml, "link"),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn test_extract_text_cdata() {
        let xml = r#"<item><description><![CDATA[Some <b>HTML</b> content]]></description></item>"#;
        assert_eq!(
            extract_text(xml, "description"),
            Some("Some <b>HTML</b> content".to_string())
        );
    }

    #[test]
    fn test_dedup_articles() {
        let articles = vec![
            Article {
                title: "GPT-5 Released".into(),
                link: "https://a.com".into(),
                summary: "From source A".into(),
                source: "Source A".into(),
                pub_date: None,
                priority: "medium".into(),
                categories: vec![],
            },
            Article {
                title: "GPT-5 Released!".into(),
                link: "https://b.com".into(),
                summary: "From source B".into(),
                source: "Source B".into(),
                pub_date: None,
                priority: "critical".into(),
                categories: vec![],
            },
        ];

        let result = dedup_articles(articles);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].priority, "critical");
    }

    #[test]
    fn test_format_unix_timestamp() {
        let ts = 1704067200; // 2024-01-01 00:00:00 UTC
        let s = format_unix_timestamp(ts);
        assert!(s.starts_with("2024-01-01"));
    }

    #[test]
    fn test_extract_atom_link() {
        let xml =
            r#"<entry><link rel="alternate" href="https://example.com/article"/></entry>"#;
        assert_eq!(
            extract_atom_link(xml),
            Some("https://example.com/article".to_string())
        );
    }

    #[test]
    fn test_filter_core_sources_pass_through() {
        let articles = vec![Article {
            title: "Unrelated Article".into(),
            link: "https://example.com".into(),
            summary: "No keywords here".into(),
            source: "OpenAI Blog".into(),
            pub_date: None,
            priority: "critical".into(),
            categories: vec![],
        }];

        let filtered = filter_by_topic(articles);
        assert_eq!(filtered.len(), 1); // Core source passes through
    }

    #[test]
    fn test_filter_non_core_needs_keywords() {
        let articles = vec![
            Article {
                title: "New AI Model Released".into(),
                link: "https://example.com/ai".into(),
                summary: "A new large language model".into(),
                source: "Random Blog".into(),
                pub_date: None,
                priority: "medium".into(),
                categories: vec![],
            },
            Article {
                title: "Best Recipes 2024".into(),
                link: "https://example.com/recipes".into(),
                summary: "Cooking tips".into(),
                source: "Random Blog".into(),
                pub_date: None,
                priority: "medium".into(),
                categories: vec![],
            },
        ];

        let filtered = filter_by_topic(articles);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].title.contains("AI"));
    }

    #[test]
    fn test_action_sources() {
        let result = action_sources();
        assert_eq!(result["feed_count"], FEEDS.len());
        assert!(result["hn_enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_tool_schema_valid() {
        let tool = NewsCuratorTool::new();
        let errors = crate::tools::tool::validate_tool_schema(
            &tool.parameters_schema(),
            "news_curator",
        );
        assert!(errors.is_empty(), "Schema errors: {:?}", errors);
    }

    #[test]
    fn test_parse_rss_feed() {
        let xml = r#"<?xml version="1.0"?>
        <rss version="2.0">
          <channel>
            <title>Test Feed</title>
            <item>
              <title>Article One</title>
              <link>https://example.com/1</link>
              <description>First article</description>
              <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
              <category>AI</category>
            </item>
            <item>
              <title>Article Two</title>
              <link>https://example.com/2</link>
              <description>Second article</description>
            </item>
          </channel>
        </rss>"#;

        let articles = parse_feed_xml(xml, "Test Feed", "high").unwrap();
        assert_eq!(articles.len(), 2);
        assert_eq!(articles[0].title, "Article One");
        assert_eq!(articles[0].source, "Test Feed");
        assert_eq!(articles[0].priority, "high");
        assert_eq!(articles[1].title, "Article Two");
    }

    #[test]
    fn test_parse_atom_feed() {
        let xml = r#"<?xml version="1.0"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test Atom Feed</title>
          <entry>
            <title>Atom Article</title>
            <link rel="alternate" href="https://example.com/atom"/>
            <summary>An atom article</summary>
            <published>2024-01-01T00:00:00Z</published>
          </entry>
        </feed>"#;

        let articles = parse_feed_xml(xml, "Atom Feed", "critical").unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Atom Article");
        assert_eq!(articles[0].link, "https://example.com/atom");
    }
}
