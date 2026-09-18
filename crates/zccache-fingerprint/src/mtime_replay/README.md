# mtime_replay

Content-verified mtime snapshot/replay (zccache#1595): `snapshot()` records
each workspace file's mtime alongside its blake3 hash, and `replay()` later
restores that mtime — but only on files whose size and hash still verify.
Every other outcome (missing, size mismatch, hash mismatch, or a failed
`set_file_times`) leaves the file's current mtime untouched, so a checkout
that changed content is never mistaken for one that didn't.
