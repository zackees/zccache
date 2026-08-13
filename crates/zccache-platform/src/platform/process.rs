//! Neutral process mechanics: spawn/detach, containment, priority, PID
//! liveness, executable-image lookup, CPU probes, stdio detach, exit/signal
//! interpretation, and jobserver pipes.
//!
//! Policy — compile-priority decisions, watchdog budgets, retry/escalation,
//! lifecycle wording, and which program to launch — stays with the callers.
//! Populated in the process phase (#1369); empty index until then.
