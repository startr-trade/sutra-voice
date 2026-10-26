//! The voice distribution's bundle: every engine transport the default features select, PLUS
//! `voip`. A dropped force-link (linker DCE) or a missing `inventory::submit!` shrinks a registry
//! and the engine would silently fail to bind that transport's channels, so the sets are pinned.

// Force-link exactly what the binary links (main.rs), so this test binary sees the same
// registrations.
#[cfg(feature = "llm-anthropic")]
use sutra_llm_anthropic as _;
#[cfg(feature = "llm-bedrock")]
use sutra_llm_bedrock as _;
#[cfg(feature = "llm-gemini")]
use sutra_llm_gemini as _;
#[cfg(feature = "llm-openai")]
use sutra_llm_openai as _;
#[cfg(feature = "amqp")]
use sutra_transport_amqp as _;
#[cfg(feature = "dapr")]
use sutra_transport_dapr as _;
#[cfg(feature = "file")]
use sutra_transport_file as _;
#[cfg(feature = "gcp-pubsub")]
use sutra_transport_gcp_pubsub as _;
#[cfg(feature = "http")]
use sutra_transport_http as _;
#[cfg(feature = "kafka")]
use sutra_transport_kafka as _;
#[cfg(feature = "knative")]
use sutra_transport_knative as _;
#[cfg(feature = "llm")]
use sutra_transport_llm as _;
#[cfg(feature = "rabbitmq")]
use sutra_transport_rabbitmq as _;
#[cfg(feature = "aws-sqs")]
use sutra_transport_sqs as _;
use sutra_transport_voip as _;

#[test]
#[allow(clippy::vec_init_then_push)] // the pushes are cfg-gated
fn voice_bundle_registers_every_enabled_transport_plus_voip() {
    let mut expected: Vec<&str> = vec!["voip"];
    #[cfg(feature = "amqp")]
    expected.push("amqp");
    #[cfg(feature = "aws-sqs")]
    expected.push("aws-sqs");
    #[cfg(feature = "dapr")]
    expected.push("dapr");
    #[cfg(feature = "file")]
    expected.push("file");
    #[cfg(feature = "gcp-pubsub")]
    expected.push("gcp-pubsub");
    #[cfg(feature = "http")]
    expected.push("http");
    #[cfg(feature = "kafka")]
    expected.push("kafka");
    #[cfg(feature = "knative")]
    expected.push("knative");
    #[cfg(feature = "llm")]
    expected.push("llm");
    #[cfg(feature = "rabbitmq")]
    expected.push("rabbitmq");
    expected.sort_unstable();

    let mut transports: Vec<&str> = sutra_transport_spi::transport_factories()
        .iter()
        .map(|f| f.transport)
        .collect();
    transports.sort_unstable();
    assert_eq!(transports, expected);
}

/// `voip` sends through the reply sink (a bare `voip://originate` is qualified with the
/// deployment) and answers channel-calls through its inline caller — without the force-link every
/// `voip://` call would have neither.
#[test]
fn voip_registers_its_reply_sink_and_inline_caller() {
    let reply: Vec<&str> = sutra_transport_spi::reply_sink_factories()
        .iter()
        .map(|f| f.transport)
        .collect();
    assert!(reply.contains(&"voip"), "{reply:?}");
    let inline: Vec<&str> = sutra_transport_spi::inline_caller_factories()
        .iter()
        .map(|f| f.transport)
        .collect();
    assert!(inline.contains(&"voip"), "{inline:?}");
}

/// The voice examples' model channels need the providers the default features bundle.
#[cfg(all(feature = "llm-openai", feature = "llm-gemini"))]
#[test]
fn voice_bundle_links_the_model_providers_the_examples_use() {
    let providers: Vec<&str> = sutra_llm_spi::provider_factories()
        .iter()
        .map(|f| f.provider)
        .collect();
    for wanted in ["openai", "gemini"] {
        assert!(
            providers.contains(&wanted),
            "{wanted} missing: {providers:?}"
        );
    }
}
