use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use frances_config::{
    ConfigEvent, ConfigHandle, ConfigProvider, Configuration, EnvProvider, EventSender, Path,
    ProviderError, TomlProvider, Value,
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
            for ev in &self.events {
                events.send(ev.clone()).await.unwrap();
            }
            Ok(())
        }
    }
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![Arc::new(EagerProvider {
        events: vec![
            ConfigEvent::new(Path::parse("a"), Value::String("1".into())),
            ConfigEvent::new(Path::parse("b"), Value::String("2".into())),
        ],
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("qwen".into()),
        ))
        .await
        .unwrap();
    // Give the processor a moment to apply.
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("first".into()),
        ))
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("second".into()),
        ))
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("hello".into()),
        ))
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("first".into()),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle
        .bind::<String>("llm::model")
        .unwrap()
        .required()
        .unwrap();
    let mut stream = binding.subscribe();
    // Unset the path. Required is sticky → no emission.
    handle
        .publish(ConfigEvent::unset(Path::parse("llm::model")))
        .await
        .unwrap();
    // Still readable.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(binding.get().as_str(), "first");
    // Set again — stream should fire now.
    handle
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("third".into()),
        ))
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
        .publish(ConfigEvent::new(
            Path::parse("llm::model"),
            Value::String("first".into()),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle.bind::<String>("llm::model").unwrap();
    let mut stream = binding.subscribe();
    handle
        .publish(ConfigEvent::unset(Path::parse("llm::model")))
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
    handle
        .publish(ConfigEvent::new(
            Path::parse("a"),
            Value::String("1".into()),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    {
        let _binding = handle.bind::<String>("a").unwrap();
    } // dropped here
    // Push another event; refresh should compact dead Weak.
    handle
        .publish(ConfigEvent::new(
            Path::parse("a"),
            Value::String("2".into()),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    // Re-bind and assert latest visible.
    let binding = handle.bind::<String>("a").unwrap().required().unwrap();
    assert_eq!(binding.get().as_str(), "2");
}

#[tokio::test]
async fn map_then_subscribe() {
    let providers: Vec<Arc<dyn ConfigProvider>> = vec![];
    let handle = ConfigHandle::build(providers).await.unwrap();
    handle
        .publish(ConfigEvent::new(Path::parse("count"), Value::Int(2)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let binding = handle.bind::<i64>("count").unwrap().required().unwrap();
    let mapped = binding.map(|n| n * 10);
    let mut stream = mapped.subscribe();
    handle
        .publish(ConfigEvent::new(Path::parse("count"), Value::Int(5)))
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("timeout")
        .expect("stream ended");
    assert_eq!(*next, 50);
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
    // Tokens still comes from TOML.
    assert_eq!(snap.get("llm::tokens").value(), Some(&Value::Int(1)));
}
