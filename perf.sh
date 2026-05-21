#!/usr/bin/env bash
# Wrapper: delegates to ci/perf.py via uv.
exec uv run python -m ci.perf "$@"
