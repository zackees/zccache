mod dashmap {
    pub struct DashMap;
    pub struct Guard;

    impl Guard {
        pub fn value(&self) {}
    }

    impl DashMap {
        pub fn new() -> Self {
            Self
        }

        pub fn insert(&self, _: &str, _: String) {}

        pub fn get(&self, _: &str) -> Option<Guard> {
            Some(Guard)
        }

        pub fn remove(&self, _: &str) {}
    }
}

use dashmap::DashMap;

fn main() {
    let cache = DashMap::new();
    cache.insert("key", String::from("value"));
    if let Some(entry) = cache.get("key") {
        let _ = entry.value();
        cache.remove("key");
    }
}
