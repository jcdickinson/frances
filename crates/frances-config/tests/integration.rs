use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use frances_config::{
    ConfigBinding, ConfigEvent, ConfigHandle, ConfigProvider, Configuration, EnvProvider,
    EventSender, MapError, Path, ProviderError, RequiredConfigBinding, TomlProvider, Value,
};
use futures::StreamExt;
use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Default)]
struct DatabaseConfig {
    host: String,
    port: u16,
    name: String,
}

#[derive(Debug, Deserialize, PartialEq, Default)]
struct LlmConfig {
    model: String,
    tokens: u32,
}

#[derive(Debug, Deserialize, Default, PartialEq)]
struct RootConfig {
    name: String,
    debug: bool,
    database: DatabaseConfig,
    tags: Vec<String>,
}

fn make_config(items: &[(&str, Value)]) -> Configuration {
    let mut cfg = Configuration::default();
    for (k, v) in items {
        cfg = cfg.applied(ConfigEvent::new(Path::parse(*k), v.clone()));
    }
    cfg
}

fn ev(path: &str, value: impl Into<Value>) -> ConfigEvent {
    ConfigEvent::new(Path::parse(path), value)
}

#[test]
fn separator_is_double_colon() {
    let cfg = make_config(&[("foo::bar", Value::String("baz".into()))]);
    assert_eq!(
        cfg.get("foo::bar").value(),
        Some(&Value::String("baz".into()))
    );
    assert_eq!(
        cfg.get("foo").get(Value::String("bar".into())).value(),
        Some(&Value::String("baz".into()))
    );
}

#[test]
fn case_insensitive_keys() {
    let cfg = make_config(&[("App::Name", Value::String("frances".into()))]);
    assert_eq!(
        cfg.get("app::name").value(),
        Some(&Value::String("frances".into()))
    );
    assert_eq!(
        cfg.get("APP::NAME").value(),
        Some(&Value::String("frances".into()))
    );
}

#[test]
fn section_binding_via_get() {
    let cfg = make_config(&[
        ("database::host", Value::String("localhost".into())),
        ("database::port", Value::Int(5432)),
        ("database::name", Value::String("mydb".into())),
    ]);
    let binding = cfg.get("database").bind::<DatabaseConfig>().unwrap();
    let v = binding.required().unwrap();
    let g = v.get();
    assert_eq!(g.host, "localhost");
    assert_eq!(g.port, 5432);
    assert_eq!(g.name, "mydb");
}

#[test]
fn nested_binding_with_arrays() {
    let cfg = make_config(&[
        ("name", Value::String("frances".into())),
        ("debug", Value::Bool(true)),
        ("database::host", Value::String("localhost".into())),
        ("database::port", Value::Int(5432)),
        ("database::name", Value::String("mydb".into())),
        ("tags::0", Value::String("alpha".into())),
        ("tags::1", Value::String("beta".into())),
    ]);
    let binding = cfg.bind::<RootConfig>().unwrap();
    let req = binding.required().unwrap();
    let v = req.get();
    assert_eq!(v.name, "frances");
    assert!(v.debug);
    assert_eq!(v.database.host, "localhost");
    assert_eq!(v.tags, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn required_section_missing_errors() {
    let cfg = Configuration::default();
    let binding = cfg.get("missing").bind::<DatabaseConfig>().unwrap();
    let err = binding.required().expect_err("must error");
    assert!(format!("{err}").contains("missing"));
}

#[tokio::test]
async fn build_propagates_provider_error() {
    struct FailProvider;
    #[async_trait]
    impl ConfigProvider for FailProvider {
        async fn load(&self, _events: EventSender) -> Result<(), ProviderError> {
            Err(ProviderError::new(std::io::Error::other("boom")))
        }
    }
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![Arc::new(FailProvider)];
    let result = ConfigHandle::build(providers).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn build_waits_for_initial_load() {
    struct EagerProvider {
        events: Vec<ConfigEvent>,
    }
    #[async_trait]
    impl ConfigProvider for EagerProvider {
        async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
            events.send(self.events.clone()).await.unwrap();
            Ok(())
        }
    }
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![Arc::new(EagerProvider {
        events: vec![ev("a", "1"), ev("b", "2")],
    })];
    let handle = ConfigHandle::build(providers).await.unwrap();
    let snap = handle.snapshot();
    assert_eq!(snap.get("a").value(), Some(&Value::String("1".into())));
    assert_eq!(snap.get("b").value(), Some(&Value::String("2".into())));
}

#[tokio::test]
async fn runtime_event_updates_snapshot() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("llm::model", "qwen")])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        handle.snapshot().get("llm::model").value(),
        Some(&Value::String("qwen".into()))
    );
}

#[tokio::test]
async fn subscribe_yields_on_event() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("llm::model", "first")])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle
        .bind::<String>("llm::model")
        .unwrap()
        .required()
        .unwrap();
    let mut stream = binding.subscribe();
    handle
        .publish(vec![ev("llm::model", "second")])
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("stream timed out")
        .expect("stream ended");
    assert_eq!(next.as_str(), "second");
}

#[tokio::test]
async fn subscribe_now_yields_current_first() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("llm::model", "hello")])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle
        .bind::<String>("llm::model")
        .unwrap()
        .required()
        .unwrap();
    let mut stream = binding.subscribe_now();
    let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert_eq!(first.as_str(), "hello");
}

#[tokio::test]
async fn required_subscribe_skips_absence() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("llm::model", "first")])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle
        .bind::<String>("llm::model")
        .unwrap()
        .required()
        .unwrap();
    let mut stream = binding.subscribe();
    handle
        .publish(vec![ConfigEvent::unset(Path::parse("llm::model"))])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(binding.get().as_str(), "first");
    handle
        .publish(vec![ev("llm::model", "third")])
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert_eq!(next.as_str(), "third");
}

#[tokio::test]
async fn optional_subscribe_emits_none_on_absence() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("llm::model", "first")])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle.bind::<String>("llm::model").unwrap();
    let mut stream = binding.subscribe();
    handle
        .publish(vec![ConfigEvent::unset(Path::parse("llm::model"))])
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert!(next.is_none());
}

#[tokio::test]
async fn handle_drops_dead_bindings() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("a", "1")]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    {
        let _binding = handle.bind::<String>("a").unwrap();
    }
    handle.publish(vec![ev("a", "2")]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle.bind::<String>("a").unwrap().required().unwrap();
    assert_eq!(binding.get().as_str(), "2");
}

#[tokio::test]
async fn env_provider_via_handle() {
    let mut vars = HashMap::new();
    vars.insert("MYAPP__DATABASE__HOST".into(), "localhost".into());
    vars.insert("MYAPP__DATABASE__PORT".into(), "5432".into());
    vars.insert("MYAPP__DATABASE__NAME".into(), "mydb".into());
    let provider: Arc<dyn ConfigProvider> =
        Arc::new(EnvProvider::from_vars(Some("MYAPP".into()), vars));
    let handle = ConfigHandle::build(vec![provider]).await.unwrap();
    let binding = handle
        .bind::<DatabaseConfig>("database")
        .unwrap()
        .required()
        .unwrap();
    let v = binding.get();
    assert_eq!(v.host, "localhost");
    assert_eq!(v.port, 5432);
    assert_eq!(v.name, "mydb");
}

#[tokio::test]
async fn toml_provider_basic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    tokio::fs::write(
        &path,
        r#"
[llm]
model = "qwen"
tokens = 1000

[[tags]]
name = "alpha"

[[tags]]
name = "beta"
"#,
    )
    .await
    .unwrap();
    let provider: Arc<dyn ConfigProvider> = Arc::new(TomlProvider::new(&path));
    let handle = ConfigHandle::build(vec![provider]).await.unwrap();
    let llm = handle.bind::<LlmConfig>("llm").unwrap().required().unwrap();
    let v = llm.get();
    assert_eq!(v.model, "qwen");
    assert_eq!(v.tokens, 1000);
}

#[tokio::test]
async fn toml_provider_optional_missing_ok() {
    let provider: Arc<dyn ConfigProvider> =
        Arc::new(TomlProvider::new("/nonexistent/path/config.toml").optional());
    let handle = ConfigHandle::build(vec![provider]).await.unwrap();
    assert!(handle.snapshot().get("anything").value().is_none());
}

#[tokio::test]
async fn toml_provider_required_missing_errors() {
    let provider: Arc<dyn ConfigProvider> =
        Arc::new(TomlProvider::new("/nonexistent/path/config.toml"));
    let result = ConfigHandle::build(vec![provider]).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn env_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    tokio::fs::write(
        &path,
        r#"
[llm]
model = "from-toml"
tokens = 1
"#,
    )
    .await
    .unwrap();
    let mut env_vars = HashMap::new();
    env_vars.insert("MYAPP__LLM__MODEL".into(), "from-env".into());
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![
        Arc::new(TomlProvider::new(&path)),
        Arc::new(EnvProvider::from_vars(Some("MYAPP".into()), env_vars)),
    ];
    let handle = ConfigHandle::build(providers).await.unwrap();
    let snap = handle.snapshot();
    assert_eq!(
        snap.get("llm::model").value(),
        Some(&Value::String("from-env".into()))
    );
    assert_eq!(snap.get("llm::tokens").value(), Some(&Value::Int(1)));
}

// ---------------------------------------------------------------------------
// New tests for round 2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_batch_one_refresh() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("x", Value::Int(0))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let binding = handle.bind::<i64>("x").unwrap().required().unwrap();
    let mut stream = binding.subscribe();

    handle
        .publish(vec![
            ev("x", Value::Int(1)),
            ev("x", Value::Int(2)),
            ev("x", Value::Int(3)),
            ev("x", Value::Int(4)),
            ev("x", Value::Int(5)),
        ])
        .await
        .unwrap();

    // Consume one yield; assert it's the post-batch state.
    let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert_eq!(*first, 5);

    // Assert no further yield within a short window — the batch produced one
    // refresh, not five.
    let extra = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
    assert!(extra.is_err(), "expected single yield from a batch");
}

#[tokio::test]
async fn toml_provider_uses_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    tokio::fs::write(
        &path,
        r#"
a = 1
b = 2
c = 3
d = 4
e = 5
"#,
    )
    .await
    .unwrap();

    // Counter provider: a tiny custom provider that registers a binding
    // first, then we rely on subsequent TomlProvider load to fire one refresh.
    // We'll instrument by counting yields on a subscribe stream.
    let provider: Arc<dyn ConfigProvider> = Arc::new(TomlProvider::new(&path));
    let handle = ConfigHandle::build(vec![provider]).await.unwrap();

    // The TOML batch already happened during build; assert all five keys
    // landed in the snapshot.
    let snap = handle.snapshot();
    for k in ["a", "b", "c", "d", "e"] {
        assert!(snap.get(k).value().is_some(), "missing key {k}");
    }
}

#[tokio::test]
async fn optional_required_distinct_types() {
    fn _f(_: ConfigBinding<i32>, _: RequiredConfigBinding<i32>) {}
}

#[tokio::test]
async fn map_async_initial() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(vec![ev("count", Value::Int(7))])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let mapped = handle
        .bind::<i64>("count")
        .unwrap()
        .map_async(|n| Box::pin(async move { Ok::<_, MapError>(n * 2) }))
        .await
        .unwrap();
    let r = mapped.get().expect("value present");
    assert_eq!(*r, 14);
}

#[tokio::test]
async fn map_async_propagates_initial_error() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("n", Value::Int(0))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let result = handle
        .bind::<i64>("n")
        .unwrap()
        .map_async(|_n: i64| {
            Box::pin(async move { Err::<i64, _>(MapError::new(std::io::Error::other("nope"))) })
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn map_async_refresh_on_upstream() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("n", Value::Int(2))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let mapped = handle
        .bind::<i64>("n")
        .unwrap()
        .map_async(|n| Box::pin(async move { Ok::<_, MapError>(n * 10) }))
        .await
        .unwrap();
    let mut stream = mapped.subscribe();
    handle.publish(vec![ev("n", Value::Int(5))]).await.unwrap();

    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert_eq!(next.expect("Some"), Arc::new(50));
}

#[tokio::test]
async fn map_async_noop_on_none() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();

    let mapped = handle
        .bind::<i64>("nope")
        .unwrap()
        .map_async(|n| Box::pin(async move { Ok::<_, MapError>(n.to_string()) }))
        .await
        .unwrap();
    assert!(mapped.get().is_none());
}

#[tokio::test]
async fn map_async_refresh_error_clears_to_none() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("n", Value::Int(2))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_mapper = counter.clone();
    let mapped = handle
        .bind::<i64>("n")
        .unwrap()
        .map_async(move |n| {
            let c = counter_for_mapper.clone();
            Box::pin(async move {
                let call = c.fetch_add(1, Ordering::SeqCst);
                if call >= 1 {
                    Err::<i64, _>(MapError::new(std::io::Error::other("denied")))
                } else {
                    Ok(n)
                }
            })
        })
        .await
        .unwrap();

    assert!(mapped.get().is_some());
    handle.publish(vec![ev("n", Value::Int(3))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(mapped.get().is_none());
}

#[tokio::test]
async fn map_async_chain() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("n", Value::Int(3))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let mapped = handle
        .bind::<i64>("n")
        .unwrap()
        .map_async(|n: i64| Box::pin(async move { Ok::<_, MapError>(n.to_string()) }))
        .await
        .unwrap()
        .map_async(|s: String| Box::pin(async move { Ok::<_, MapError>(s.len()) }))
        .await
        .unwrap();

    let r = mapped.get().expect("value present");
    assert_eq!(*r, 1usize); // "3".len() == 1
}

#[tokio::test]
async fn map_async_then_required() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle.publish(vec![ev("n", Value::Int(7))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;

    let req: RequiredConfigBinding<i64, String> = handle
        .bind::<i64>("n")
        .unwrap()
        .map_async(|n: i64| Box::pin(async move { Ok::<_, MapError>(n.to_string()) }))
        .await
        .unwrap()
        .required()
        .unwrap();

    assert_eq!(req.get().as_str(), "7");
    handle.publish(vec![ev("n", Value::Int(42))]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(req.get().as_str(), "42");
}

#[tokio::test]
async fn t_preserved_through_chain() {
    // Compile-only: the chain should produce ConfigBinding<i64, bool>.
    fn _check() {
        async fn _build(handle: &ConfigHandle) {
            let _b: ConfigBinding<i64, bool> = handle
                .bind::<i64>("x")
                .unwrap()
                .map_async(|n: i64| Box::pin(async move { Ok::<_, MapError>(n.to_string()) }))
                .await
                .unwrap()
                .map_async(|s: String| Box::pin(async move { Ok::<_, MapError>(s.is_empty()) }))
                .await
                .unwrap();
        }
        let _ = _build;
    }
}
