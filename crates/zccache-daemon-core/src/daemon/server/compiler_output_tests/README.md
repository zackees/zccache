# Compiler output tests

This directory holds focused tests split from the parent compiler-output
module so the production implementation stays within the repository
source-file size limit.

`tests.rs` covers target-specific sidecars included in cached compiler output
sets, including packed Linux DWARF and MSVC program databases.
