#[global_allocator]
static GLOBAL: mimalloc_pprof::MiMalloc = mimalloc_pprof::MiMalloc;

fn main() {
    zccache::download_daemon_entry::run();
}
