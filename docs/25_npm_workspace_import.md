# npm workspace gate import

`frost import-npm` discovers npm workspaces from the root `package.json` and
turns selected non-interactive package scripts into Frost `test` targets.
This is the first bounded step toward issue #87: package/script discovery and
cacheable validation gates, without pretending that Frost can already own a
Vite or other dynamic output tree.

Preview the generated root manifest:

```bash
frost -C my-monorepo import-npm --dry-run
```

The default script set is `test` and `typecheck`. Select another
non-interactive gate by repeating or comma-separating `--script`:

```bash
frost -C my-monorepo import-npm \
  --script test,typecheck,e2e \
  --npm /absolute/path/to/npm \
  --node /absolute/path/to/node
frost -C my-monorepo test --all
```

`lint` is opt-in because some framework-provided lint scripts bootstrap or
rewrite project configuration on their first run. Import it only after the
workspace's lint command is known to be non-interactive and non-mutating:

```bash
frost -C my-monorepo import-npm --script test,typecheck,lint
```

The importer supports both npm workspace forms:

```json
{ "workspaces": ["apps/*", "packages/*"] }
```

```json
{ "workspaces": { "packages": ["apps/*", "packages/*"] } }
```

For every matching package script, the generated target:

- invokes `npm run SCRIPT --workspace PACKAGE` as direct argv through a
  fingerprinted `[toolchain.tools].npm`, with the Node runtime included in the
  same toolchain closure;
- is a first-class Frost test gate with success-only result caching;
- tracks the root package metadata, npm lock/config files, the package tree,
  and transitive in-repository workspace dependency trees;
- links same-script runtime workspace dependencies in the Frost target graph;
- forces `CI=true` so imported gates cannot silently enter watch mode, and
  forwards only an explicit small Node/npm environment set;
- disables the workspace sandbox because `node_modules` remains npm-owned.

The generated broad package patterns rely on the repository `.gitignore` and
`.frostignore` to exclude `node_modules`, `dist`, coverage, compiler caches,
and other generated state. Review the manifest before running it.

## Deliberate limits

The importer does not import `build`, `dev`, or every discovered script by
default. A package script can be interactive, start a persistent process, or
write a variable output tree; treating any of those as a cached test would be
incorrect.

In particular, Vite `dist/` remains npm/Vite-owned. Frost command outputs are
declared files with digest-verified ownership, so an npm/Vite build still needs
an explicit stable artifact boundary or future dynamic output-tree support.
The importer never guesses `dist/index.html`, never treats a directory as a
file output, and never overwrites an existing `frost.toml`.

This scope improves real monorepo validation and affected-gate pruning, but it
does not by itself complete issue #87. Persistent compiler/browser HMR,
hermetic `node_modules`, source-map debugging, dynamic output ownership, and
an adopted production-repository build remain separate work.
