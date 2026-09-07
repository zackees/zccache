#[global_allocator]
static GLOBAL: kernal_api::allocator::Allocator = kernal_api::allocator::Allocator::new();

fn main() {
    zccache::download_daemon_entry::run();
}
