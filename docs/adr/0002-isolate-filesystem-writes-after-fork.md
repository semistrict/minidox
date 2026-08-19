# Isolate filesystem writes after a fork

Every VM owns a Filesystem Branch. Forking derives the Child VM's branch copy-on-write from the Source VM at the Fork Point, so later filesystem writes in either VM are isolated while unchanged pages remain physically shared. Branch ancestry is owned independently of individual VM lifetimes: a child remains valid and forkable after its source exits. Branches may fork recursively but do not merge, rebase, or propagate later writes to relatives. A fork never creates an implicitly shared mutable filesystem namespace.
