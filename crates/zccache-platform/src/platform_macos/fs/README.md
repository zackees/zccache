# platform_macos::fs — macOS filesystem mechanics

Concrete leaves: identity (st_dev + st_ino), links (st_nlink, symlink
classification), permissions (mode 0700 tighten, mode-bit restore),
replace (rename(2)), volume (st_dev), path (/private prefix
canonicalization, case-insensitive folding), positioned_io (pread/pwrite).
