# Source

The lint matches resolved `std::env::var` and `std::env::var_os` calls whose
literal or resolved `&str` constant argument evaluates to a registered
owned-boolean name. Evaluating the value follows re-exports and local aliases
without mistaking an unrelated same-named constant for policy. The policy owner
is the only production source excluded from the rule.
