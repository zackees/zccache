# zccache-download-protocol

Protocol messages (`Request`/`Response`) and shared daemon-management utilities for the zccache download daemon.

The download daemon retains its own length-prefixed bincode framing, separate
from the main daemon's prost IPC. Payloads are capped at 1 MiB before buffering
or encoding.
