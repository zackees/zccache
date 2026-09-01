mod zccache_core {
    pub mod config {
        pub const CACHE_TEST_BINS_ENV: &str = "ZCCACHE_CACHE_TEST_BINS";
    }
}

const LOCAL_ALIAS: &str = zccache_core::config::CACHE_TEST_BINS_ENV;
const CACHE_TEST_BINS_ENV: &str = "ZCCACHE_NOT_REGISTERED";

fn main() {
    let _ = std::env::var("ZCCACHE_DISABLE");
    let _ = std::env::var_os(zccache_core::config::CACHE_TEST_BINS_ENV);
    let _ = std::env::var(LOCAL_ALIAS);
    let _ = std::env::var(CACHE_TEST_BINS_ENV);
}
