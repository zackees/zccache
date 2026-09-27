# Dylint target proof

`--features violate` activates one `ban_std_pathbuf` finding for each of the
eight published target triples. CI checks that the selected target's negative
pass fails with the named custom lint, then that the clean pass succeeds.
