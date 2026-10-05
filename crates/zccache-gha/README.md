# zccache-gha

GitHub Actions cache client used by zccache.

This crate preserves the former `zccache::gha` module surface so the main
crate can re-export it behind the `gha` feature.

`GhaCache::restore` builds its query string with URL encoding (`keys` and
`version` are appended as query parameters, never interpolated), so keys
containing `#`, `&`, or spaces address the entry the caller asked for instead of
a truncated or hijacked query.
