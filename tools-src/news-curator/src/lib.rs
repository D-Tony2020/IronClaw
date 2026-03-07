//! News Curator WASM Tool for IronClaw.
//!
//! Fetches tech news from RSS feeds and Hacker News, deduplicates by title,
//! filters by topic keywords, and returns a structured JSON array.
//!
//! The LLM agent (newsbot) uses this tool to gather raw articles, then
//! compiles them into a Chinese digest using its skill prompt.

wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const USER_AGENT: &str = "IronClaw-NewsCurator/1.0";
const DEFAULT_LOOKBACK_HOURS: u64 = 48;
const DEFAULT_MAX_ARTICLES: usize = 50;
const HN_TOP_STORIES_URL: &str = "https://hacker-news.firebaseio.com/v0/topstories.json";
const HN_ITEM_URL: &str = "https://hacker-news.firebaseio.com/v0/item";
const MAX_HN_STORIES: usize = 30;
const MAX_SUMMARY_LEN: usize = 500;
const HTTP_TIMEOUT_MS: u32 = 15_000;

// ---------------------------------------------------------------------------
// WIT bindings
// ---------------------------------------------------------------------------

struct NewsCuratorTool;

impl exports::near::agent::tool::Guest for NewsCuratorTool {
    fn execute(req: exports::near::agent::tool::Request) -> exports::near::agent::tool::Response {
        match execute_inner(&req.params) {
            Ok(result) => exports::near::agent::tool::Response {
                output: Some(result),
                error: None,
            },
            Err(e) => exports::near::agent::tool::Response {
                output: None,
                error: Some(e),
            },
        }
    }

    fn schema() -> String {
        SCHEMA.to_string()
    }

    fn description() -> String {
        "Fetch, deduplicate, and filter tech news from curated RSS feeds and \
         Hacker News. Returns a JSON array of articles with title, link, \
         summary, source, priority, and categories. Use the 'fetch' action \
         to collect articles, or 'sources' to list configured feeds."
            .to_string()
    }
}

export!(NewsCuratorTool);

// ---------------------------------------------------------------------------
// Parameters & types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Params {
    action: String,
    #[serde(default)]
    max_articles: Option<usize>,
    #[serde(default)]
    lookback_hours: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
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

// ---------------------------------------------------------------------------
// RSS feed definitions (hardcoded from news-sources.yaml)
// ---------------------------------------------------------------------------

struct FeedDef {
    name: &'static str,
    url: &'static str,
    priority: &'static str,
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
// Main execution
// ---------------------------------------------------------------------------

fn execute_inner(params_str: &str) -> Result<String, String> {
    let params: Params =
        serde_json::from_str(params_str).map_err(|e| format!("Invalid parameters: {e}"))?;

    match params.action.as_str() {
        "fetch" => action_fetch(params),
        "sources" => action_sources(),
        _ => Err(format!(
            "Unknown action '{}'. Valid actions: fetch, sources",
            params.action
        )),
    }
}

fn action_fetch(params: Params) -> Result<String, String> {
    let max_articles = params.max_articles.unwrap_or(DEFAULT_MAX_ARTICLES);
    let _lookback_hours = params.lookback_hours.unwrap_or(DEFAULT_LOOKBACK_HOURS);

    log_info("Starting news fetch...");

    // 1. Fetch RSS feeds
    let mut all_articles: Vec<Article> = Vec::new();
    let mut feed_stats: HashMap<&str, (usize, usize)> = HashMap::new(); // name → (fetched, errors)

    for feed in FEEDS {
        match fetch_rss_feed(feed) {
            Ok(articles) => {
                let count = articles.len();
                feed_stats.insert(feed.name, (count, 0));
                all_articles.extend(articles);
            }
            Err(e) => {
                feed_stats.insert(feed.name, (0, 1));
                log_warn(&format!("Feed '{}' failed: {}", feed.name, e));
            }
        }
    }

    // 2. Fetch Hacker News top stories
    match fetch_hacker_news() {
        Ok(hn_articles) => {
            let count = hn_articles.len();
            feed_stats.insert("Hacker News API", (count, 0));
            all_articles.extend(hn_articles);
        }
        Err(e) => {
            feed_stats.insert("Hacker News API", (0, 1));
            log_warn(&format!("Hacker News fetch failed: {e}"));
        }
    }

    let total_raw = all_articles.len();
    log_info(&format!("Raw articles collected: {total_raw}"));

    // 3. Deduplicate by normalized title
    let before_dedup = all_articles.len();
    all_articles = dedup_articles(all_articles);
    log_info(&format!(
        "After dedup: {} (removed {} duplicates)",
        all_articles.len(),
        before_dedup - all_articles.len()
    ));

    // 4. Filter by topic keywords
    let before_filter = all_articles.len();
    all_articles = filter_by_topic(all_articles);
    log_info(&format!(
        "After topic filter: {} (removed {} off-topic)",
        all_articles.len(),
        before_filter - all_articles.len()
    ));

    // 5. Sort by priority then recency
    sort_articles(&mut all_articles);

    // 6. Truncate to max
    all_articles.truncate(max_articles);

    // 7. Build output
    let feeds_ok: usize = feed_stats.values().filter(|(_, e)| *e == 0).count();
    let feeds_err: usize = feed_stats.values().filter(|(_, e)| *e > 0).count();

    let output = serde_json::json!({
        "article_count": all_articles.len(),
        "total_raw": total_raw,
        "feeds_ok": feeds_ok,
        "feeds_error": feeds_err,
        "articles": all_articles,
    });

    serde_json::to_string(&output).map_err(|e| format!("Serialization failed: {e}"))
}

fn action_sources() -> Result<String, String> {
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

    let output = serde_json::json!({
        "feed_count": FEEDS.len(),
        "feeds": sources,
        "hn_enabled": true,
        "topic_keyword_count": TOPIC_KEYWORDS.len(),
    });

    serde_json::to_string(&output).map_err(|e| format!("Serialization failed: {e}"))
}

// ---------------------------------------------------------------------------
// RSS/Atom parsing (lightweight, no external XML lib)
// ---------------------------------------------------------------------------

fn fetch_rss_feed(feed: &FeedDef) -> Result<Vec<Article>, String> {
    let headers = serde_json::json!({
        "User-Agent": USER_AGENT,
        "Accept": "application/rss+xml, application/atom+xml, application/xml, text/xml"
    });

    let resp = near::agent::host::http_request(
        "GET",
        feed.url,
        &headers.to_string(),
        None,
        Some(HTTP_TIMEOUT_MS),
    )
    .map_err(|e| format!("HTTP error: {e}"))?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!("HTTP {}", resp.status));
    }

    let body = String::from_utf8(resp.body)
        .map_err(|_| "Invalid UTF-8 response".to_string())?;

    parse_feed_xml(&body, feed)
}

/// Parse RSS 2.0 or Atom XML into articles.
///
/// Handles both `<item>` (RSS) and `<entry>` (Atom) elements.
fn parse_feed_xml(xml: &str, feed: &FeedDef) -> Result<Vec<Article>, String> {
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
            // Atom: <link href="..." /> or <link href="...">...</link>
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
            source: feed.name.to_string(),
            pub_date,
            priority: feed.priority.to_string(),
            categories,
        });
    }

    Ok(articles)
}

// ---------------------------------------------------------------------------
// Hacker News fetch
// ---------------------------------------------------------------------------

fn fetch_hacker_news() -> Result<Vec<Article>, String> {
    let headers = serde_json::json!({ "User-Agent": USER_AGENT });
    let headers_str = headers.to_string();

    // 1. Get top story IDs
    let resp = near::agent::host::http_request(
        "GET",
        HN_TOP_STORIES_URL,
        &headers_str,
        None,
        Some(HTTP_TIMEOUT_MS),
    )
    .map_err(|e| format!("HN top stories: {e}"))?;

    if resp.status != 200 {
        return Err(format!("HN HTTP {}", resp.status));
    }

    let body = String::from_utf8(resp.body).map_err(|_| "Invalid UTF-8".to_string())?;
    let ids: Vec<u64> =
        serde_json::from_str(&body).map_err(|e| format!("HN parse IDs: {e}"))?;

    // 2. Fetch individual stories (limit to MAX_HN_STORIES)
    let mut articles = Vec::new();

    for &id in ids.iter().take(MAX_HN_STORIES) {
        let url = format!("{HN_ITEM_URL}/{id}.json");
        match near::agent::host::http_request("GET", &url, &headers_str, None, Some(10_000)) {
            Ok(resp) if resp.status == 200 => {
                if let Ok(body) = String::from_utf8(resp.body) {
                    if let Ok(story) = serde_json::from_str::<HnStory>(&body) {
                        if let Some(title) = story.title {
                            let link = story
                                .url
                                .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={id}"));
                            let score = story.score.unwrap_or(0);
                            let priority = if score > 200 { "high" } else { "medium" };

                            let summary = format!(
                                "HN score: {} | comments: {}",
                                score,
                                story.descendants.unwrap_or(0)
                            );

                            articles.push(Article {
                                title,
                                link,
                                summary,
                                source: "Hacker News".to_string(),
                                pub_date: story.time.map(|t| {
                                    // Unix timestamp → ISO-ish string
                                    format_unix_timestamp(t)
                                }),
                                priority: priority.to_string(),
                                categories: vec!["tech".to_string()],
                            });
                        }
                    }
                }
            }
            _ => continue,
        }
    }

    Ok(articles)
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
    // Core-tier feeds pass through without filtering (they're already curated)
    let core_sources: HashSet<&str> = [
        "OpenAI Blog", "Anthropic Research", "Google DeepMind",
        "Google Research", "Meta AI / FAIR", "Qwen 通义千问", "机器之心",
    ]
    .into_iter()
    .collect();

    articles
        .into_iter()
        .filter(|a| {
            if core_sources.contains(a.source.as_str()) {
                return true;
            }
            let haystack = format!(
                "{} {} {}",
                a.title,
                a.summary,
                a.categories.join(" ")
            )
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
// XML helpers (minimal, no external lib)
// ---------------------------------------------------------------------------

/// Extract all occurrences of `<tag>...</tag>` from XML.
fn extract_elements(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut results = Vec::new();
    let mut search_from = 0;

    while let Some(start) = xml[search_from..].find(&open) {
        let abs_start = search_from + start;
        // Find the end of the opening tag (handle attributes)
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
    // Skip to end of opening tag (past attributes)
    let after_open = xml[start_pos..].find('>')? + start_pos + 1;
    // Handle CDATA
    let content_start = if xml[after_open..].starts_with("<![CDATA[") {
        after_open + 9
    } else {
        after_open
    };

    let end_pos = xml[content_start..].find(&close)? + content_start;

    let mut content = &xml[content_start..end_pos];

    // Strip CDATA end if present
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
    // Look for <link ... href="..." ...> preferring rel="alternate"
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

    // Normalize whitespace
    let decoded = decode_xml_entities(&result);
    decoded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

/// Format a Unix timestamp into a human-readable date string.
fn format_unix_timestamp(ts: u64) -> String {
    let secs = ts;
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let mins = (remaining % 3600) / 60;

    // Simple epoch → date (days since 1970-01-01)
    let (year, month, day) = days_to_date(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:00Z")
}

/// Convert days since epoch to (year, month, day).
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
// Logging helpers
// ---------------------------------------------------------------------------

fn log_info(msg: &str) {
    near::agent::host::log(near::agent::host::LogLevel::Info, msg);
}

fn log_warn(msg: &str) {
    near::agent::host::log(near::agent::host::LogLevel::Warn, msg);
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"{
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
            "maximum": 200,
            "default": 50
        },
        "lookback_hours": {
            "type": "integer",
            "description": "How many hours back to fetch (default 48)",
            "minimum": 1,
            "maximum": 168,
            "default": 48
        }
    },
    "required": ["action"],
    "additionalProperties": false
}"#;

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
        assert_eq!(
            clean_html("<p>Hello <b>world</b></p>"),
            "Hello world"
        );
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
        let xml = r#"<item><title>Test Article</title><link>https://example.com</link></item>"#;
        assert_eq!(extract_text(xml, "title"), Some("Test Article".to_string()));
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
        assert_eq!(result[0].priority, "critical"); // Higher priority wins
    }

    #[test]
    fn test_format_unix_timestamp() {
        // 2024-01-01 00:00:00 UTC
        let ts = 1704067200;
        let s = format_unix_timestamp(ts);
        assert!(s.starts_with("2024-01-01"));
    }

    #[test]
    fn test_extract_atom_link() {
        let xml = r#"<entry><link rel="alternate" href="https://example.com/article"/></entry>"#;
        assert_eq!(
            extract_atom_link(xml),
            Some("https://example.com/article".to_string())
        );
    }
}
