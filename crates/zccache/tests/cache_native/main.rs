//! Native-compiler cache integration tests (#1526).
//!
//! Covers PCH, DLL, link and response-file caching, the ninja/meson
//! rebuild routes, the clang-cl / MSVC / Arduino classification surface,
//! and generic exec. Each former top-level `tests/*.rs` file is a module
//! here, so the category links one test executable instead of 17. Test
//! IDs are `cache_native::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod compiler_arduino_ino;
mod compiler_clang_cl_classification;
mod compiler_response_file_expansion_test;
mod compiler_response_file_integration_test;
mod compiler_response_file_parser_test;
mod daemon_burst_link_test;
mod daemon_dll_cache_test;
mod daemon_generic_exec_advanced_test;
mod daemon_generic_exec_test;
mod daemon_link_cache_test;
mod daemon_msvc_cl_cache_test;
mod daemon_ninja_rebuild_direct_test;
mod daemon_ninja_rebuild_meson_test;
mod daemon_pch_cache_basic_test;
mod daemon_pch_cache_invalidation_test;
mod daemon_response_file_cache;
mod link_bundle_integration_test;
