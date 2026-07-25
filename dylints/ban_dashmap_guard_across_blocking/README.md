# ban_dashmap_guard_across_blocking

This Dylint rejects direct `DashMap::get` guards that remain live while their
`if let` body waits, touches the filesystem or a process, or mutates a map.

Clone the guarded value into an owned local before performing that work.
