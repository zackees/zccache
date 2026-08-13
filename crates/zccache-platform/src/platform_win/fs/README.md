# platform_win::fs — Windows filesystem mechanics

Concrete leaves: identity (volume serial + 128-bit FileIdInfo), links
(CreateFileW link counts, reparse classification), permissions (owner-only
DACLs, file attributes), replace (MoveFileExW + verbatim long paths),
volume (GetVolumeInformationW), path (verbatim-prefix stripping, case
folding, MSYS conversion), positioned_io (OVERLAPPED pread/pwrite).

Windows-only tests (128-bit identity preservation, DACL readback, reparse
classification, USN change-marker safety) live here, not in the neutral
facade.
