#!/usr/bin/env bash
# Reproduce a single failing test the way CI runs it.
#
# NOTE: Skill scripts are NOT executed by the runtime in Phase 2 — they are
# recorded and displayed only. Execution waits for the sandbox (Phase 6), which
# is why a skill that ships scripts is registered as non-executable so retrieval
# never selects a script-dependent behaviour. This file is a reference.
set -euo pipefail

test_name="${1:?usage: reproduce.sh <test_name>}"

cargo test "$test_name" -- --exact
