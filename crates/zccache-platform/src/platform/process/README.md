# Process facade

Neutral, statically dispatched host-process mechanics. Product decisions such
as compile priority selection, watchdog budgets, daemon identity, logging, and
diagnostic wording remain in their owning crates.

Leaves cover command preparation, spawning, priority, inspection,
termination, standard-I/O detachment, native jobserver primitives, and native
exit/crash interpretation.
