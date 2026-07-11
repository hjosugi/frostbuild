<!-- i18n: language-switcher -->
[English](01_architecture_nix_bazel_micro_partition.md) | [日本語](01_architecture_nix_bazel_micro_partition.ja.md)

# Nix + Bazel + Micro-partition architecture

## Core idea

```text
Nix layer:
  exact environment and toolchain closure

Bazel/Buck layer:
  action graph, dependency graph, remote cache, remote execution

Snowflake layer:
  metadata catalog and partition pruning
```

The new tool is not just a build runner. It is closer to a query engine for builds.

```text
input:
  git diff
  requested target
  platform
  environment
  cache state

planner:
  affected partition pruning
  dependency expansion
  test selection
  cache lookup
  worker placement

executor:
  local/remote parallel actions
  CAS artifact store
  lazy materialization
```

## Why Nix matters

Nix builds packages in isolation to avoid undeclared dependencies. That is the exact property we want for a correct action environment.

For every build action, include an environment hash:

```text
action_key = hash(
  command,
  args,
  declared_inputs,
  environment_closure_hash,
  platform,
  selected_env_vars
)
```

This gives us correctness across machines.

## Why Bazel/Buck matter

Bazel and Buck2 teach us that a build is a graph of small actions.

```text
target -> actions -> inputs -> outputs
```

A fast build system should:

```text
- know the graph
- run independent actions in parallel
- cache outputs by action key
- use remote workers when local CPU is insufficient
```

## Why micro-partitions matter

Snowflake micro-partitions store metadata that lets the query engine skip irrelevant data. The build equivalent is:

```text
source file / module / target / test = micro-partition
metadata = imports, exports, hashes, reverse deps, test coverage, cost
pruning = skip unrelated build/test work
```

Example:

```text
changed:
  packages/ui/Button.tsx

normal project-level affected:
  packages/ui
  apps/web
  many web tests

micro-partition affected:
  Button partition
  pages that import Button
  tests that cover Button
```

## Metadata catalog

A real implementation should store:

```text
partition_id
language
package
path_prefix
source_files
source_hashes
imports
exports
reverse_deps
toolchain_hash
config_hash
env_hash
output_digest
tests_covering_this
last_duration_ms
last_failure_rate
worker_cache_location
owner
```

Local prototype:

```text
.frost/metadata.json
.frost/action_cache.json
.frost/cas/<digest>
```

Production:

```text
metadata catalog: Postgres / FoundationDB / SQLite replicated
artifact store:   S3 / GCS / R2 / CAS service
remote exec:      REAPI-compatible workers
local state:      RocksDB / SQLite
```

## Query planner analogy

SQL query engine:

```text
predicate -> partition pruning -> execution plan -> workers
```

Build engine:

```text
git diff + target -> build partition pruning -> action plan -> workers
```

This is the main design difference from normal build tools.

## Correctness rule

Never skip work unless one of these is true:

```text
1. the partition is proven unaffected by dependency metadata
2. the exact action key has a cache hit
3. the user explicitly accepts probabilistic test selection risk
```

For production, the default mode should be conservative.

```text
safe mode:
  over-select affected work
  never under-select

fast mode:
  use probabilistic test selection
  require nightly/full validation
```
