mod dashmap {
    #[derive(Clone)]
    pub struct DashMap;

    impl DashMap {
        pub fn new() -> Self {
            Self
        }

        pub fn insert(&self, _: &str, _: String) {}

        pub fn get(&self, _: &str) -> Option<Self> {
            Some(Self)
        }

        pub fn remove(&self, _: &str) {}
    }
}

use dashmap::DashMap;

fn main() {
    let cache = DashMap::new();
    cache.insert("key", String::from("value"));
    let entry = cache.get("key").map(|entry| entry.clone());
    if let Some(entry) = entry {
        let _ = entry;
        cache.remove("key");
    }
}
