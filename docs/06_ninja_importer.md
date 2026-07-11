# Ninja Importer

`frost import-ninja build.ninja --output frost.toml` converts a small, explicit
Ninja subset into `frost.toml`. The Python reference command remains available
for comparison.
The goal is migration smoke testing for CMake-generated graphs, not complete Ninja
compatibility.

Supported:

- top-level `var = value` assignments
- `rule NAME` blocks with `command = ...`
- `build OUT: RULE IN...`
- `build NAME: phony OUT...` as a default-target reference only
- `default OUT...`
- `$var` and `${var}` expansion

Unsupported by design in the v1 importer:

- pools, dyndep, rspfile, depfile attributes, validations, includes, subninja
- implicit and order-only dependency semantics beyond token preservation
- per-edge variable bindings
- shell-compatible command lowering into real process execution

Unsupported/unknown rules raise an error instead of silently producing an
incorrect graph. The importer emits genrule dependencies for inputs produced by
another Ninja edge and preserves other inputs as declared files.
