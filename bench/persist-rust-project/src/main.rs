fn main() {
    let _ = serde_json::to_string(&42);
    let _ = regex::Regex::new("x").unwrap();
    let _ = url::Url::parse("http://x");
    let _ = indexmap::IndexMap::<u8, u8>::new();
    let _ = bytes::Bytes::new();
    let _ = itertools::Itertools::collect_vec(std::iter::empty::<u8>());
    let _: Result<(), anyhow::Error> = Ok(());
    let _ = hex::encode([0u8; 4]);
    let _ = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"x");
    once_cell::sync::Lazy::force(&once_cell::sync::Lazy::new(|| ()));
    log::info!("ok");
    #[derive(thiserror::Error, Debug)]
    #[error("noop")]
    struct E;
    let _ = E;
}
