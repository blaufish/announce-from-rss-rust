use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bsky_sdk::api::app::bsky::embed::external::{ExternalData, MainData as ExternalMainData};
use bsky_sdk::api::app::bsky::feed::defs::PostViewEmbedRefs;
use bsky_sdk::api::app::bsky::feed::get_author_feed::ParametersData as FeedParams;
use bsky_sdk::api::app::bsky::feed::post::{RecordData as PostRecordData, RecordEmbedRefs};
use bsky_sdk::api::types::string::{AtIdentifier, Datetime};
use bsky_sdk::api::types::Union;
use bsky_sdk::BskyAgent;
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use serde::Deserialize;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "rss-bsky-bot",
    about = "RSS announcer bot for Bluesky",
    after_help = "Hope this was helpful! :-)"
)]
struct Cli {
    /// Configuration file location
    #[arg(long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize, Default)]
struct LimitsConfig {
    days: Option<i64>,
    posts: Option<usize>,
    dryrun: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct BskySecrets {
    password: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct BskyConfig {
    enabled: Option<bool>,
    handle: Option<String>,
    secrets: Option<BskySecrets>,
}

#[derive(Debug, Deserialize, Default)]
struct BotConfig {
    loglevel: Option<String>,
    url: Option<String>,
    transformer: Option<String>,
    limits: Option<LimitsConfig>,
    bsky: Option<BskyConfig>,
}

struct Limits {
    max_days: i64,
    max_posts: usize,
    dryrun: bool,
}

impl From<&Option<LimitsConfig>> for Limits {
    fn from(cfg: &Option<LimitsConfig>) -> Self {
        let cfg = cfg.as_ref();
        Self {
            max_days: cfg.and_then(|c| c.days).unwrap_or(1),
            max_posts: cfg.and_then(|c| c.posts).unwrap_or(1),
            dryrun: cfg.and_then(|c| c.dryrun).unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    title: String,
    description: String,
    link: String,
    published: DateTime<Utc>,
}

fn setup_logging(level: Option<&str>) {
    let raw = level.unwrap_or("INFO").to_lowercase();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(raw));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn fetch_candidates(url: &str, threshold: DateTime<Utc>) -> Result<Vec<Candidate>> {
    info!("Request feed from: {url}");
    let body = reqwest::get(url)
        .await
        .with_context(|| format!("Failed to fetch RSS feed from {url}"))?
        .bytes()
        .await
        .context("Failed to read RSS response body")?;

    let feed = feed_rs::parser::parse(&body[..]).context("Failed to parse RSS/Atom feed")?;
    let mut candidates = Vec::new();

    for entry in feed.entries {
        let link = entry
            .links
            .first()
            .map(|l| l.href.clone())
            .unwrap_or_default();
        let title = entry.title.map(|t| t.content).unwrap_or_default();
        let description = entry
            .summary
            .map(|s| s.content)
            .or_else(|| entry.content.and_then(|c| c.body))
            .unwrap_or_default();

        let published = match entry.published.or(entry.updated) {
            Some(dt) => dt,
            None => {
                debug!("RSS skipping entry without timestamp: {title}");
                continue;
            }
        };

        if published > threshold {
            info!("RSS candidate: {title}");
            candidates.push(Candidate {
                title,
                description,
                link,
                published,
            });
        } else {
            debug!("RSS skipping old entry: {title}");
        }
    }

    Ok(candidates)
}

fn transform_sakerhetspodcasten(candidates: &mut [Candidate]) {
    for candidate in candidates {
        let cleaned = candidate
            .description
            .replace("Lyssna mp3, längd: ", "")
            .replace(" Innehåll ", " ");
        candidate.description = cleaned;
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

async fn recently_posted_uris(agent: &BskyAgent, did: &str) -> Result<HashSet<String>> {
    let actor: AtIdentifier = did
        .parse()
        .map_err(|e: &str| anyhow::anyhow!("Invalid DID: {e}"))?;

    let feed = agent
        .api
        .app
        .bsky
        .feed
        .get_author_feed(
            FeedParams {
                actor,
                filter: Some("posts_and_author_threads".to_string()),
                include_pins: None,
                limit: Some(30.try_into().unwrap()),
                cursor: None,
            }
            .into(),
        )
        .await
        .context("Failed to fetch author feed")?;

    let mut posted = HashSet::new();

    for item in &feed.feed {
        if let Some(Union::Refs(PostViewEmbedRefs::AppBskyEmbedExternalView(ext_view))) =
            &item.post.embed
        {
            let uri = ext_view.external.uri.clone();
            debug!("Bluesky embedded: {uri}");
            posted.insert(uri);
        }
    }

    Ok(posted)
}

async fn post_candidate(agent: &BskyAgent, candidate: &Candidate, dryrun: bool) -> Result<()> {
    let c_title = &candidate.title;
    let c_description = html_escape::decode_html_entities(&candidate.description).into_owned();
    let c_uri = &candidate.link;

    debug!("c_title: {c_title}");
    debug!("c_description: {c_description}");
    debug!("c_uri: {c_uri}");

    let raw_text = format!("📣 {c_title} 📣 {c_description}");
    let text = truncate_chars(&raw_text, 300);

    if dryrun {
        info!("Dry-run post: {c_uri}");
        return Ok(());
    }

    info!("Post: {c_uri}");

    let embed = Union::Refs(RecordEmbedRefs::AppBskyEmbedExternalMain(Box::new(
        ExternalMainData {
            external: ExternalData {
                title: c_title.clone(),
                description: c_description,
                uri: c_uri.clone(),
                thumb: None,
                associated_refs: None,
            }
            .into(),
        }
        .into(),
    )));

    let res = agent
        .create_record(PostRecordData {
            created_at: Datetime::now(),
            embed: Some(embed),
            entities: None,
            facets: None,
            labels: None,
            langs: None,
            reply: None,
            tags: None,
            text,
        })
        .await
        .context("Failed to create Bluesky post")?;

    info!("post.uri: {}", res.uri);
    info!("post.cid: {:?}", res.cid);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let raw_yaml = fs::read_to_string(&cli.config)
        .with_context(|| format!("Failed to read config file {}", cli.config.display()))?;
    let config: BotConfig = serde_yaml::from_str(&raw_yaml).context("Failed to parse YAML config")?;

    setup_logging(config.loglevel.as_deref());
    debug!("Configuration loaded: {config:?}");

    let Some(url) = config.url.as_deref() else {
        error!("No \"url\" to RSS channel in config file!");
        return Ok(());
    };

    let bsky_cfg = match config.bsky.as_ref() {
        Some(b) if b.enabled.unwrap_or(true) => b,
        Some(_) => {
            info!("Bot implementation bsky is disabled.");
            return Ok(());
        }
        None => {
            info!("Bot implementation bsky not configured.");
            return Ok(());
        }
    };

    let Some(handle) = bsky_cfg.handle.as_deref() else {
        error!("Error retrieving: bsky.handle");
        return Ok(());
    };

    let Some(password_variable_name) = bsky_cfg.secrets.as_ref().and_then(|s| s.password.as_deref()) else {
        error!("Error retrieving: bsky.secrets.password");
        return Ok(());
    };

    let password = std::env::var(password_variable_name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            bsky_cfg
                .secrets
                .as_ref()
                .and_then(|s| s.password.as_deref())
                .filter(|s| !s.starts_with("${"))
                .map(str::to_owned)
        });

    let Some(password) = password else {
        error!("Error retrieving Bluesky password from environment");
        return Ok(());
    };

    let limits = Limits::from(&config.limits);
    if limits.dryrun {
        info!("Dryrun: announcements are inhibited.");
    }
    info!("Limit: max announcements: {}.", limits.max_posts);
    info!("Limit: max days: {}.", limits.max_days);

    let threshold = Utc::now() - Duration::days(limits.max_days);
    let mut candidates = fetch_candidates(url, threshold).await?;

    if candidates.is_empty() {
        info!(
            "No new RSS entries within the last {} day(s), exiting!",
            limits.max_days
        );
        return Ok(());
    }

    if let Some(transformer) = config.transformer.as_deref() {
        match transformer {
            "sakerhetspodcasten" => transform_sakerhetspodcasten(&mut candidates),
            other => {
                error!("Unknown transformer: {other}.");
                return Ok(());
            }
        }
    }

    info!("Bluesky bot implementation: startup");
    let agent = BskyAgent::builder()
        .build()
        .await
        .context("Failed to build BskyAgent")?;
    let session = agent
        .login(handle, password)
        .await
        .context("Failed to log in to Bluesky")?;

    let did = session.did.as_str();
    info!("Bluesky lookup: {handle}={did}");

    let posted = recently_posted_uris(&agent, did).await?;
    let mut posts_count = 0usize;

    for candidate in &candidates {
        if posts_count >= limits.max_posts {
            info!("Stopping posting after reaching post limit: {posts_count}");
            break;
        }

        if posted.contains(&candidate.link) {
            info!("Disregard already published: {}", candidate.link);
            continue;
        }

        debug!("Prepare post: {}", candidate.link);
        if let Err(err) = post_candidate(&agent, candidate, limits.dryrun).await {
            warn!("Failed to post {}: {err:#}", candidate.link);
            continue;
        }
        posts_count += 1;
    }

    info!("Terminating normally. Thanks for All the Fish!");
    Ok(())
}
