//! Shared helpers for integration tests: deterministic generated bodies,
//! automatically cleaned temporary directories, and authoritative servers
//! that touch nothing of the host.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use toker::catalog::FetchedCatalogs;
use toker::config::Config;
use toker::server::{Seams, Server};
use toker::store::Store;

/// The models each fetched-catalogue source lists: `("openrouter",
/// &["z-ai/glm-5.3"])`. Sources are `openrouter`, `openai_api`, `anthropic`
/// and `codex_sub`.
pub type Listing<'a> = (&'static str, &'a [&'a str]);

/// Deterministic startup catalogues listing exactly these models, with no
/// windows or prices. A server built over them never reads the host's cache.
pub fn catalogs(listings: &[Listing]) -> FetchedCatalogs {
    FetchedCatalogs::listing(listings.iter().copied())
}

/// An authoritative server over `store`, revision one established from the
/// config's routing fields, with no sleep lock, an empty in-memory keyring
/// and no catalogues: the same as production minus everything of the host.
pub fn server(config: Config, store: &Arc<Store>) -> Server {
    server_with(config, store, Seams::isolated())
}

/// [`server`], with these catalogues present from startup.
pub fn server_with_models(config: Config, store: &Arc<Store>, listings: &[Listing]) -> Server {
    server_with(
        config,
        store,
        Seams::isolated().with_catalogs(catalogs(listings)),
    )
}

/// [`server`], with the seams the test needs (a fake sleep-lock spawner, a
/// seeded keyring, catalogues).
pub fn server_with(config: Config, store: &Arc<Store>, seams: Seams) -> Server {
    Server::with_seams(config, store.clone(), seams).expect("build server")
}

/// Serve the router on a loopback port for the life of the test runtime.
pub async fn serve(server: Server) -> SocketAddr {
    let app = server.router();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("toker binds");
    let addr = listener.local_addr().expect("toker addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("toker serves");
    });
    addr
}

pub struct TestDir(tempfile::TempDir);

impl std::ops::Deref for TestDir {
    type Target = std::path::Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        self.0.path()
    }
}

pub fn tempdir(prefix: &str) -> TestDir {
    let root = std::path::Path::new("/tmp/opencode");
    std::fs::create_dir_all(root).expect("create test scratch root");
    TestDir(
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .expect("create temporary test directory"),
    )
}

/// splitmix64: small, deterministic, plenty to vary generated bodies.
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

pub const WORDS: &[&str] = &[
    "alpha",
    "bravo",
    "charlie",
    "delta",
    "echo",
    "foxtrot",
    "golf",
    "hotel",
    "india",
    "juliet",
    "kilo",
    "lima",
    "Wellington",
    "café",
    "日本語",
    "🎉",
];
const MODELS: &[&str] = &[
    "openai/gpt-5.2",
    "z-ai/glm-5.3",
    "anthropic/claude-sonnet-4.6",
    "qwen/qwen4-max",
];
const TOOL_NAMES: &[&str] = &["read_file", "list_dir", "run_command"];

pub fn conversation_body(rng: &mut Rng, message_count: usize) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert("model".to_owned(), serde_json::json!(rng.pick(MODELS)));
    if rng.below(2) == 0 {
        map.insert("stream".to_owned(), serde_json::json!(true));
    }
    if rng.below(2) == 0 {
        map.insert("temperature".to_owned(), serde_json::json!(0.7));
    }
    if rng.below(3) == 0 {
        let tools: Vec<_> = (0..1 + rng.below(2))
            .map(|_| {
                serde_json::json!({
                    "type": "function",
                    "function": {"name": rng.pick(TOOL_NAMES)},
                })
            })
            .collect();
        map.insert("tools".to_owned(), serde_json::Value::Array(tools));
    }
    let mut messages = Vec::with_capacity(message_count);
    for i in 0..message_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let words = 1 + rng.below(5) as usize;
        let content = (0..words)
            .map(|_| rng.pick(WORDS))
            .collect::<Vec<_>>()
            .join(" ");
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    map.insert("messages".to_owned(), serde_json::Value::Array(messages));
    serde_json::to_vec(&serde_json::Value::Object(map)).expect("serialise generated body")
}

const ANTHROPIC_MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-4.6", "claude-haiku-4.5"];
const ANTHROPIC_SYSTEMS: &[&str] = &[
    "You are a careful assistant.",
    "Be terse and verify every claim.",
    "Prefer canonical JSON output.",
];

pub fn anthropic_conversation_body(rng: &mut Rng, message_count: usize) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert(
        "model".to_owned(),
        serde_json::json!(rng.pick(ANTHROPIC_MODELS)),
    );
    if rng.below(2) == 0 {
        map.insert(
            "system".to_owned(),
            serde_json::json!(rng.pick(ANTHROPIC_SYSTEMS)),
        );
    }
    if rng.below(3) == 0 {
        let tools: Vec<_> = (0..1 + rng.below(2))
            .map(|_| {
                serde_json::json!({
                    "name": rng.pick(TOOL_NAMES),
                    "input_schema": {"type": "object"},
                })
            })
            .collect();
        map.insert("tools".to_owned(), serde_json::Value::Array(tools));
    }
    if rng.below(2) == 0 {
        map.insert("stream".to_owned(), serde_json::json!(true));
    }
    if rng.below(3) == 0 {
        map.insert("max_tokens".to_owned(), serde_json::json!(1024));
    }
    let mut messages = Vec::with_capacity(message_count);
    for i in 0..message_count {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let words = 1 + rng.below(5) as usize;
        let content = (0..words)
            .map(|_| rng.pick(WORDS))
            .collect::<Vec<_>>()
            .join(" ");
        messages.push(serde_json::json!({"role": role, "content": content}));
    }
    map.insert("messages".to_owned(), serde_json::Value::Array(messages));
    serde_json::to_vec(&serde_json::Value::Object(map)).expect("serialise generated body")
}
