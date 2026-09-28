# Dylint target proof

`--features violate` activates one `ban_std_pathbuf` finding for each of the
eight published target triples. For each OS leg Dylint runs (Linux GNU x64,
Windows MSVC x64, macOS arm64; #1740), CI checks that the negative pass fails
with the named custom lint, then that the clean pass succeeds.
