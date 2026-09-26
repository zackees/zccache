# CI test fixtures

This directory contains small, checked-in response bodies used by the CI
tests. The cache budget tests use them to cover repositories below and above
the configured GitHub Actions cache limit without making live API requests.

Keep fixtures representative of the API response shape. Tests should read
these files as immutable input and should not rewrite them.
