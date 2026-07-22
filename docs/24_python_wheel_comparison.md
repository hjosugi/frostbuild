# Python wheel comparison

This gate asks a narrow, useful question: how quickly can each frontend turn
the same pure-Python source-layout package into a valid wheel? It does not call
source copying a Python build, and it does not claim to replace arbitrary PEP
517 build backends.

## Contract

The format contract follows the
[PyPA binary distribution specification](https://packaging.python.org/en/latest/specifications/binary-distribution-format/),
including its filename, metadata and `RECORD` rules. `frost-bench python`
generates one distribution with 100 value modules and an
`__init__.py` entry point (101 Python sources total). Frost uses its built-in
`pack-wheel`; the other frontends both invoke the same installed
`setuptools.build_meta` backend:

```text
python -m build --wheel --no-isolation
uv build --wheel --no-build-isolation --offline
```

The uv invocation uses its documented
[`build` interface](https://docs.astral.sh/uv/reference/cli/#uv-build); offline
mode makes accidental dependency resolution a hard failure.

After every timed build, the harness checks:

- the exact installed Python source names and bytes;
- distribution `Name` and `Version`;
- wheel version, purelib setting and `py3-none-any` tag;
- one valid SHA-256/size `RECORD` row for every archive entry, with the
  self-record left unhashed;
- successful execution after extracting the wheel, with exact stdout and
  empty stderr.

Backend-specific optional metadata, generator identity and ZIP layout are not
required to be byte-identical. The report records both semantic and complete
archive digests, making that distinction machine-checkable.

The wheel writer itself sorts inputs, uses stable ZIP metadata, rejects
symlinks and unsafe paths, filters bytecode/cache files, writes through a
temporary archive and atomically publishes the result. The required
`METADATA`, `WHEEL` and `RECORD` entries are placed last, with `RECORD`
physically last.

## Checked result

The committed
`bench/baselines/2026-07-21-E14-python-wheel.json` report uses Frost 0.2.0,
Python 3.14.6, build 1.5.0, uv 0.11.29, 101 sources, alternating frontend
order and seven samples per scenario. Medians are milliseconds:

| Frontend | clean | unchanged invocation | one source change |
|---|---:|---:|---:|
| Frost | 21.295 | 2.600 | 7.806 |
| `python -m build` | 766.806 | 619.512 | 612.785 |
| `uv build` | 326.911 | 290.841 | 290.786 |

Against the faster incumbent (`uv`), Frost measured 15.35x faster clean,
111.86x unchanged and 37.25x after one source change. All three semantic
digests match. The complete Frost wheel differs because it intentionally emits
only the minimum contract metadata while setuptools also emits optional
metadata; the setuptools wheels produced through build and uv are byte
identical in this run.

The starting one-minute load average was 2.00 on an eight-logical-CPU host,
and is retained in the report. The gaps are large, but the result remains one
checked packaging boundary rather than a universal Python build claim.

## Why it wins, and where it does not

The Frost action performs one in-process-free native invocation: hash declared
inputs, write the standardized archive, then publish it. `build` and `uv` must
enter PEP 517 and start the Python backend even when no source changed. Frost's
unchanged path skips the action entirely.

That advantage applies when the distribution is pure Python and its metadata
is representable by the built-in packer. The following remain open:

- arbitrary PEP 517 backend hooks and dynamically computed metadata;
- dependencies, entry points, licenses, package-data selection and editable
  installs;
- C/Rust extension modules and platform-specific wheel tags;
- sdist generation and publication/signing workflows;
- import-graph partitioning, pytest collection/affected selection, type
  checking, linting and coverage integration.

For those cases, use a normal `command` target around the owner tool. Frost can
still provide a fast declared project boundary, but must not describe that as
making the backend itself faster.

## Reproduce

```bash
cargo build --release --locked
FROST_BIN=target/release/frost ./frost-bench python \
  --tools frost,python-build,uv \
  --size 100 --iterations 7 --jobs 4 \
  --out bench/baselines/<date>-<host>-python-wheel.json
```

Build isolation is disabled for both incumbent frontends so environment setup
and network resolution are not charged to either one. The installed backend,
runtime and uv cache are retained; clean samples remove only each workspace's
wheel, backend build tree, egg-info and Frost state.
