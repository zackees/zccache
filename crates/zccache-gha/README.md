# zccache-gha

GitHub Actions cache client used by zccache.

This crate preserves the former `zccache::gha` module surface so the main
crate can re-export it behind the `gha` feature.
