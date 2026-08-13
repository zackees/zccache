// Everything in this file is legal under the zccache#1365 boundary:
// host-independent cfgs are allowed anywhere.

fn cfg_test_macro() -> bool {
    cfg!(test)
}

#[cfg(test)]
fn private_test_item() {}

#[cfg(feature = "anything")]
fn private_feature_item() {}

#[cfg(debug_assertions)]
fn private_debug_item() {}

#[cfg_attr(test, allow(unused_mut))]
fn private_cfg_attr_test_item() {}

fn main() {
    let _ = cfg_test_macro();
}
