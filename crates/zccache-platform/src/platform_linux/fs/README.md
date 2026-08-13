# platform_linux::fs — Linux filesystem mechanics

Concrete leaves: identity (st_dev + st_ino), links (st_nlink, symlink
classification), permissions (mode 0700 tighten, mode-bit restore),
replace (rename(2)), volume (st_dev), path (case-sensitive no-ops),
positioned_io (pread/pwrite).
