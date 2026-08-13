//! Neutral host facts: OS/architecture identity, home/runtime directories,
//! current-user identity, elevation state, Defender query/mutation
//! primitives, and host resource/pressure facts.
//!
//! Cache-root precedence, first-run UX, Defender policy wording, and
//! scheduling thresholds stay with the callers. Populated in the host phase
//! (#1370); empty index until then.
