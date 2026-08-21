# Python Package Tests

Public-package and installed-native-extension contracts for the combined
`zccache` wheel. Tests that require native modules skip in a pure-source
checkout; the release workflow runs the installed-wheel smoke on Linux,
macOS, and Windows before publishing.
