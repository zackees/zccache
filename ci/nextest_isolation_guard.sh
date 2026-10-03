#!/bin/sh
# Nextest run-wrapper: zccache's test suite never runs on a developer host
# (zackees/ci.yml#168, GATE-005).
#
# zccache is live infrastructure on every developer machine: soldr routes
# every compile through its daemon. The tests start zccache daemons and touch
# zccache state roots, so a leaked fixture on a host run can claim or wedge
# the real cache (the same failure took down soldr's ~/.soldr root in
# zackees/soldr#3516). CI=true covers GitHub runners and act;
# ZCCACHE_TEST_ISOLATED=1 is set only by the isolated bosn gate image
# (ci/docker/gate/Dockerfile).
if [ "${CI:-}" != "true" ] && [ "${ZCCACHE_TEST_ISOLATED:-}" != "1" ]; then
    echo "zccache tests refuse to run on a developer host (zackees/ci.yml#168, GATE-005)." >&2
    echo "Run them isolated: bosn run --task gate-test" >&2
    exit 97
fi
exec "$@"
