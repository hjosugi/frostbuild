//! End-to-end tests driving the real `frost` binary against the sample_c
//! workspace with the host C compiler.

use std::path::{Path, PathBuf};
use std::process::Command;

fn frost_bin() -> &'static str {
    env!("CARGO_BIN_EXE_frost")
}

struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample_c");
        let workspace = Self::empty(name);
        copy_dir(&src, &workspace.dir).expect("copy sample_c");
        workspace
    }

    fn empty(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("frost-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create empty workspace");
        Self { dir }
    }

    fn frost(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(frost_bin())
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .expect("spawn frost");
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    fn frost_env(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let mut command = Command::new(frost_bin());
        command.arg("-C").arg(&self.dir).args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        let out = command.output().expect("spawn frost");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr),
        )
    }

    fn build_explain(&self) -> (bool, String) {
        self.frost(&["build", "--explain"])
    }

    fn write(&self, rel: &str, content: &str) {
        std::fs::write(self.dir.join(rel), content).unwrap();
    }

    fn append(&self, rel: &str, content: &str) {
        let path = self.dir.join(rel);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(content);
        std::fs::write(&path, text).unwrap();
    }

    fn run_app(&self) -> String {
        let out = Command::new(self.dir.join(".frost/bin/debug/app"))
            .output()
            .expect("run built app");
        assert!(out.status.success(), "built app should run");
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        // Never inherit build state from the checked-out sample workspace;
        // every test must start from a genuinely clean tree even if someone
        // ran frost against sample_c manually.
        if entry.file_name() == ".frost" {
            continue;
        }
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn kofun_binary_builds_incrementally_and_hits_the_action_cache() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::empty("kofun");
    std::fs::create_dir_all(ws.dir.join("src")).unwrap();
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["alpha", "beta"]

[toolchain]
kofunc = "tools/kofunc"

[target.alpha]
kind = "kofun_binary"
srcs = ["src/alpha.kofun"]

[target.beta]
kind = "kofun_binary"
srcs = ["src/beta.kofun"]
"#,
    );
    ws.write("src/alpha.kofun", "alpha-v1\n");
    ws.write("src/beta.kofun", "beta-v1\n");
    ws.write(
        "tools/kofunc",
        r#"#!/bin/sh
set -eu
test "$1" = build
source=$2
test "$3" = -o
output=$4
test "$5" = --emit-c
emitted=$6
printf '%s\n' "$source" >> compiler.log
value=$(sed -n '1p' "$source")
printf '/* generated from %s */\n' "$value" > "$emitted"
printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$value" > "$output"
chmod +x "$output"
"#,
    );
    std::fs::set_permissions(
        ws.dir.join("tools/kofunc"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let (ok, initial) = ws.build_explain();
    assert!(ok, "initial Kofun build failed:\n{initial}");
    assert!(initial.contains("ran kofun:alpha"), "{initial}");
    assert!(initial.contains("ran kofun:beta"), "{initial}");
    assert!(initial.contains("2 built"), "{initial}");
    for target in ["alpha", "beta"] {
        assert!(
            ws.dir.join(format!(".frost/bin/debug/{target}")).is_file(),
            "{target} binary was not produced"
        );
        assert!(
            ws.dir
                .join(format!(".frost/obj/debug/{target}/kofun.c"))
                .is_file(),
            "{target} emitted C was not declared and retained"
        );
    }

    let invocations = || std::fs::read_to_string(ws.dir.join("compiler.log")).unwrap();
    let mut initial_invocations = invocations()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    initial_invocations.sort();
    assert_eq!(
        initial_invocations,
        ["src/alpha.kofun", "src/beta.kofun"],
        "independent targets may execute in either scheduler order"
    );

    let (ok, unchanged) = ws.build_explain();
    assert!(ok, "unchanged Kofun rebuild failed:\n{unchanged}");
    assert!(unchanged.contains("up to date"), "{unchanged}");
    assert!(!unchanged.contains("  ran "), "{unchanged}");
    assert_eq!(
        invocations().lines().count(),
        2,
        "cached actions must not invoke kofunc"
    );

    ws.write("src/alpha.kofun", "alpha-v2\n");
    let (ok, incremental) = ws.build_explain();
    assert!(ok, "incremental Kofun build failed:\n{incremental}");
    assert!(
        incremental.contains("ran kofun:alpha :: input changed: src/alpha.kofun"),
        "{incremental}"
    );
    assert!(
        !incremental.contains("ran kofun:beta"),
        "unaffected Kofun target recompiled:\n{incremental}"
    );
    assert!(incremental.contains("1 built, 1 cached"), "{incremental}");
    let after_edit = invocations()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(after_edit.len(), 3);
    assert_eq!(
        after_edit
            .iter()
            .filter(|source| source.as_str() == "src/alpha.kofun")
            .count(),
        2
    );
    assert_eq!(
        after_edit
            .iter()
            .filter(|source| source.as_str() == "src/beta.kofun")
            .count(),
        1
    );

    let alpha = Command::new(ws.dir.join(".frost/bin/debug/alpha"))
        .output()
        .expect("run Kofun shim output");
    assert!(alpha.status.success());
    assert_eq!(String::from_utf8_lossy(&alpha.stdout), "alpha-v2\n");

    let (ok, final_noop) = ws.build_explain();
    assert!(ok && final_noop.contains("up to date"), "{final_noop}");
    assert_eq!(invocations().lines().count(), 3);
}

#[test]
fn platforms_isolate_outputs_and_caches() {
    let ws = Workspace::new("platforms");
    ws.append(
        "frost.toml",
        "\n[platform.devsim]\ncflags = [\"-DDEVICE=1\"]\n",
    );

    let (ok, out) = ws.build_explain();
    assert!(ok, "host build failed:\n{out}");

    let (ok, out) = ws.frost(&["build", "--platform", "devsim", "--explain"]);
    assert!(ok, "devsim build failed:\n{out}");
    assert!(
        out.contains("5 built"),
        "platform build must not reuse host action results:\n{out}"
    );
    assert!(
        ws.dir.join(".frost/bin/devsim/debug/app").exists(),
        "platform binary lives in a platform-segmented tree"
    );
    assert!(
        ws.dir.join(".frost/bin/debug/app").exists(),
        "host binary keeps its historical path"
    );

    // Both configurations stay warm simultaneously: switching back and
    // forth is a cache lookup, never a rebuild (the Bazel analysis-cache
    // wipe pain, avoided by keying every action on its configuration).
    let (ok, out) = ws.frost(&["build", "--platform", "devsim", "--explain"]);
    assert!(ok && out.contains("up to date"), "{out}");
    let (ok, out) = ws.build_explain();
    assert!(ok && out.contains("up to date"), "{out}");
}

#[test]
fn unknown_platform_fails_with_diagnostic() {
    let ws = Workspace::new("unknown-platform");
    let (ok, out) = ws.frost(&["build", "--platform", "nope"]);
    assert!(!ok, "build with undeclared platform must fail");
    assert!(out.contains("unknown platform"), "{out}");
}

/// Real device cross-compilation: build the sample workspace for
/// aarch64-linux-musl via `zig cc` and verify the produced ELF machine.
/// Skips (with a note) when zig is not installed.
#[test]
#[cfg(unix)]
fn cross_compile_aarch64_device_build() {
    if Command::new("zig").arg("version").output().is_err() {
        eprintln!("skipping cross_compile_aarch64_device_build: zig not in PATH");
        return;
    }
    let ws = Workspace::new("cross-aarch64");
    ws.write(
        "tools/zig-cc",
        "#!/bin/sh\nexec zig cc -target aarch64-linux-musl \"$@\"\n",
    );
    ws.write("tools/zig-ar", "#!/bin/sh\nexec zig ar \"$@\"\n");
    for tool in ["tools/zig-cc", "tools/zig-ar"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(ws.dir.join(tool), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    ws.append(
        "frost.toml",
        "\n[platform.aarch64]\ncc = \"tools/zig-cc\"\nar = \"tools/zig-ar\"\n",
    );

    let (ok, out) = ws.frost(&["build", "--platform", "aarch64", "--explain"]);
    assert!(ok, "cross build failed:\n{out}");

    let bin = std::fs::read(ws.dir.join(".frost/bin/aarch64/debug/app")).unwrap();
    assert_eq!(&bin[..4], b"\x7fELF", "output must be an ELF binary");
    let machine = u16::from_le_bytes([bin[18], bin[19]]);
    assert_eq!(machine, 183, "e_machine must be EM_AARCH64 (183)");

    // Cross results are cached independently of the host tree.
    let (ok, out) = ws.frost(&["build", "--platform", "aarch64", "--explain"]);
    assert!(ok && out.contains("up to date"), "{out}");
}

#[test]
fn query_deps_rdeps_somepath() {
    let ws = Workspace::new("query");

    let (ok, out) = ws.frost(&["query", "deps", "app"]);
    assert!(ok, "{out}");
    assert_eq!(
        out.trim().lines().collect::<Vec<_>>(),
        ["app", "gen_config", "util"]
    );

    let (ok, out) = ws.frost(&["query", "rdeps", "util"]);
    assert!(ok, "{out}");
    assert_eq!(out.trim().lines().collect::<Vec<_>>(), ["app", "util"]);

    let (ok, out) = ws.frost(&["query", "somepath", "app", "gen_config", "--json"]);
    assert!(ok, "{out}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed["query"], "somepath(app, gen_config)");
    assert_eq!(parsed["targets"][0], "app");

    let (ok, out) = ws.frost(&["query", "somepath", "util", "gen_config"]);
    assert!(!ok, "no-path case exits nonzero");
    assert!(out.contains("no path"), "{out}");
}

#[test]
fn clean_build_then_noop_is_fully_cached() {
    let ws = Workspace::new("noop");

    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");
    assert!(out.contains("5 built"), "unexpected summary:\n{out}");
    assert_eq!(ws.run_app(), "frost: 42\n");

    let (ok, out) = ws.build_explain();
    assert!(ok, "no-op build failed:\n{out}");
    assert!(
        out.contains("up to date"),
        "no-op should be fully cached:\n{out}"
    );
    assert!(
        !out.contains("  ran "),
        "no actions should have run:\n{out}"
    );
}

#[test]
fn internal_header_change_recompiles_only_util() {
    let ws = Workspace::new("header");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    // util_internal.h is only included by util.c; discovered via the depfile.
    ws.write(
        "src/util_internal.h",
        "#ifndef FROST_SAMPLE_UTIL_INTERNAL_H\n\
         #define FROST_SAMPLE_UTIL_INTERNAL_H\n\
         #define FROST_INTERNAL_BIAS 1\n\
         #endif\n",
    );

    let (ok, out) = ws.build_explain();
    assert!(ok, "incremental build failed:\n{out}");
    assert!(
        out.contains("ran compile:util:src/util.c :: input changed: src/util_internal.h"),
        "util.c should recompile due to the header:\n{out}"
    );
    assert!(
        !out.contains("ran compile:app:src/main.c"),
        "main.c must NOT recompile for an internal header change:\n{out}"
    );
    assert!(out.contains("ran archive:util"), "{out}");
    assert!(out.contains("ran link:app"), "{out}");
    assert_eq!(ws.run_app(), "frost: 43\n");
}

#[test]
fn genrule_rerun_with_identical_output_cuts_off_downstream() {
    let ws = Workspace::new("cutoff");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    // Touching the script changes the genrule's key, but the regenerated
    // header is byte-identical, so downstream compiles must stay cached.
    ws.append("tools/gen_config.sh", "# harmless tweak\n");

    let (ok, out) = ws.build_explain();
    assert!(ok, "incremental build failed:\n{out}");
    assert!(out.contains("ran genrule:gen_config"), "{out}");
    assert!(
        out.contains("1 built, 4 cached"),
        "early cutoff should keep downstream cached:\n{out}"
    );
}

#[test]
fn cflags_change_recompiles_translation_units() {
    let ws = Workspace::new("flags");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    let manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    ws.write(
        "frost.toml",
        &manifest.replace(
            "cflags = [\"-O2\", \"-Wall\"]",
            "cflags = [\"-O2\", \"-Wall\", \"-DFROST_EXTRA=1\"]",
        ),
    );

    let (ok, out) = ws.build_explain();
    assert!(ok, "incremental build failed:\n{out}");
    assert!(
        out.contains("ran compile:util:src/util.c :: command or toolchain changed"),
        "{out}"
    );
    assert!(
        out.contains("ran compile:app:src/main.c :: command or toolchain changed"),
        "{out}"
    );
}

#[test]
fn deleted_output_is_rebuilt() {
    let ws = Workspace::new("tamper");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    std::fs::remove_file(ws.dir.join(".frost/bin/debug/app")).unwrap();

    let (ok, out) = ws.build_explain();
    assert!(ok, "rebuild failed:\n{out}");
    assert!(out.contains("up to date"), "CAS restore expected:\n{out}");
    assert_eq!(ws.run_app(), "frost: 42\n");
}

#[test]
fn plan_predicts_and_build_settles() {
    let ws = Workspace::new("plan");

    let (ok, out) = ws.frost(&["plan"]);
    assert!(ok, "plan failed:\n{out}");
    assert!(out.contains("would run genrule:gen_config"), "{out}");
    assert!(
        out.contains("may run"),
        "downstream should be may-run:\n{out}"
    );

    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "build failed:\n{out}");

    let (ok, out) = ws.frost(&["plan"]);
    assert!(ok, "plan failed:\n{out}");
    assert!(
        out.contains("plan: 0 would run, 0 may run, 5 cached"),
        "{out}"
    );
}

#[test]
fn compile_failure_reports_and_skips_downstream() {
    let ws = Workspace::new("fail");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    ws.write("src/util.c", "#include \"util.h\"\nthis is not C\n");

    let (ok, out) = ws.build_explain();
    assert!(!ok, "build must fail on a compile error");
    assert!(out.contains("FAILED: CC src/util.c"), "{out}");
    assert!(out.contains("failed"), "{out}");
    assert!(
        out.contains("skipped link:app") || out.contains("skipped archive:util"),
        "downstream must be skipped:\n{out}"
    );
}

#[test]
fn clean_removes_outputs_and_full_rebuild_works() {
    let ws = Workspace::new("clean");
    let (ok, out) = ws.build_explain();
    assert!(ok, "clean build failed:\n{out}");

    let (ok, out) = ws.frost(&["clean"]);
    assert!(ok, "clean failed:\n{out}");
    assert!(!ws.dir.join(".frost/bin/debug/app").exists());
    assert!(!ws.dir.join("gen/config.h").exists());

    let (ok, out) = ws.build_explain();
    assert!(ok, "rebuild after clean failed:\n{out}");
    assert!(
        out.contains("up to date") && out.contains("5 actions"),
        "the CAS should restore the outputs rather than rebuild them:\n{out}"
    );
}

#[test]
fn graph_dot_lists_target_edges() {
    let ws = Workspace::new("graph");
    let (ok, out) = ws.frost(&["graph", "--dot"]);
    assert!(ok, "graph failed:\n{out}");
    assert!(out.contains("\"app\" -> \"util\""), "{out}");
    assert!(out.contains("digraph frost"), "{out}");
}

#[test]
fn profiles_coexist_and_switch_back_is_cached() {
    let ws = Workspace::new("profiles");
    let mut manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    manifest.push_str(
        "\n[profile.debug]\ncflags = [\"-g\"]\n\n[profile.release]\ncflags = [\"-O3\"]\n",
    );
    ws.write("frost.toml", &manifest);
    let (ok, out) = ws.frost(&["build", "--profile", "debug"]);
    assert!(ok, "{out}");
    let (ok, out) = ws.frost(&["build", "--profile", "release"]);
    assert!(ok, "{out}");
    assert!(ws.dir.join(".frost/bin/debug/app").exists());
    assert!(ws.dir.join(".frost/bin/release/app").exists());
    let (ok, out) = ws.frost(&["build", "--profile", "debug"]);
    assert!(ok && out.contains("up to date"), "{out}");
}

#[test]
fn cxx_glob_test_and_compdb_are_usable() {
    let ws = Workspace::new("cxx-test");
    ws.write("src/math.cpp", "int answer() { return 42; }\n");
    ws.write(
        "src/math_test.cpp",
        "extern int answer(); int main() { return answer() == 42 ? 0 : 1; }\n",
    );
    let mut manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    manifest.push_str("\n[target.math_test]\nkind = \"cc_test\"\nsrcs = [\"src/math*.cpp\"]\n");
    ws.write("frost.toml", &manifest);
    let (ok, out) = ws.frost(&["test", "math_test", "--explain"]);
    assert!(ok && out.contains("tests: 1 passed"), "{out}");
    let (ok, out) = ws.frost(&["test", "math_test"]);
    assert!(ok && out.contains("1 cached"), "{out}");
    let (ok, out) = ws.frost(&["compdb"]);
    assert!(ok, "{out}");
    let db: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ws.dir.join("compile_commands.json")).unwrap())
            .unwrap();
    assert!(db
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["file"] == "src/math.cpp"));
}

#[test]
fn test_all_selects_every_test_target() {
    let ws = Workspace::new("test-all");
    ws.append(
        "frost.toml",
        "\n[target.first]\nkind = \"test\"\ncmd = \"true\"\n\
         \n[target.second]\nkind = \"test\"\ncmd = \"true\"\n",
    );

    // An explicit target would normally select only that target. `--all`
    // intentionally expands the selection to every declared test target.
    let (ok, out) = ws.frost(&["test", "first", "--all", "--no-cache"]);
    assert!(ok && out.contains("tests: 2 passed"), "{out}");

    let (ok, out) = ws.frost(&["test", "--all", "--predictive"]);
    assert!(!ok && out.contains("cannot be used with"), "{out}");
}

#[test]
fn multi_package_labels_build_across_packages() {
    let ws = Workspace::new("packages");
    std::fs::create_dir_all(ws.dir.join("lib")).unwrap();
    std::fs::create_dir_all(ws.dir.join("app")).unwrap();
    ws.write(
        "frost.toml",
        "[workspace]\ndefault_targets = [\"//app:app\"]\n",
    );
    ws.write("lib/lib.c", "int package_value(void) { return 7; }\n");
    ws.write(
        "lib/frost.toml",
        "[target.lib]\nkind = \"cc_library\"\nsrcs = [\"lib.c\"]\n",
    );
    ws.write(
        "app/main.c",
        "int package_value(void); int main(void) { return package_value() == 7 ? 0 : 1; }\n",
    );
    ws.write(
        "app/frost.toml",
        "[target.app]\nkind = \"cc_binary\"\nsrcs = [\"main.c\"]\ndeps = [\"//lib:lib\"]\n",
    );
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "{out}");
    let status = Command::new(ws.dir.join(".frost/bin/debug/app_app"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn generated_header_is_order_only_for_unrelated_translation_units() {
    let ws = Workspace::new("order-only");
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    let script = std::fs::read_to_string(ws.dir.join("tools/gen_config.sh")).unwrap();
    ws.write("tools/gen_config.sh", &script.replace("frost:", "ice:"));
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    assert!(out.contains("ran compile:app:src/main.c"), "{out}");
    assert!(
        !out.contains("ran compile:util:src/util.c"),
        "unrelated TU rebuilt:\n{out}"
    );
    assert_eq!(ws.run_app(), "ice: 42\n");
}

#[test]
fn determinism_check_names_macro_and_output() {
    let ws = Workspace::new("determinism");
    ws.write(
        "src/nondeterministic.c",
        "const char *stamp = __TIME__; int main(void) { return stamp[0] == 0; }\n",
    );
    let mut manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    manifest.push_str(
        "\n[target.nondeterministic]\nkind = \"cc_binary\"\nsrcs = [\"src/nondeterministic.c\"]\n",
    );
    ws.write("frost.toml", &manifest);
    let (ok, out) = ws.frost(&["build", "nondeterministic", "--check-determinism"]);
    assert!(!ok, "nondeterministic action must fail the check");
    assert!(
        out.contains("non-deterministic action compile:nondeterministic"),
        "{out}"
    );
    assert!(out.contains(".frost/obj/debug/nondeterministic"), "{out}");
}

#[test]
fn daemon_build_status_and_stop() {
    let ws = Workspace::new("daemon");
    let (ok, out) = ws.frost(&["build", "--daemon"]);
    assert!(ok, "{out}");
    let (ok, out) = ws.frost(&["build", "--daemon"]);
    assert!(ok && out.contains("up to date"), "{out}");

    // The daemon must not trust only watcher state: build outputs live under
    // .frost (which the watcher intentionally ignores). The engine still has
    // to validate outputs and restore a manually deleted artifact from CAS.
    std::fs::remove_file(ws.dir.join(".frost/bin/debug/app")).unwrap();
    let (ok, out) = ws.frost(&["build", "--daemon", "--explain"]);
    assert!(ok && out.contains("up to date"), "{out}");
    assert_eq!(ws.run_app(), "frost: 42\n");

    ws.append("src/util.c", "\n/* daemon watcher change */\n");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let (ok, out) = ws.frost(&["build", "--daemon"]);
    assert!(
        ok && out.contains("1 built"),
        "source change missed:\n{out}"
    );
    let (ok, out) = ws.frost(&["daemon", "status"]);
    assert!(ok && out.contains("running"), "{out}");
    let (ok, out) = ws.frost(&["daemon", "stop"]);
    assert!(ok && out.contains("stopped"), "{out}");
}

#[test]
fn completed_action_survives_killed_build() {
    let ws = Workspace::new("journal-kill");
    ws.write(
        "frost.toml",
        "[workspace]\ndefault_targets = [\"slow\"]\n\n[target.fast]\nkind = \"genrule\"\ncmd = \"printf done > ${out}\"\noutputs = [\"gen/fast.txt\"]\n\n[target.slow]\nkind = \"genrule\"\ncmd = \"sleep 10; printf done > ${out}\"\noutputs = [\"gen/slow.txt\"]\ndeps = [\"fast\"]\n",
    );
    let mut child = Command::new(frost_bin())
        .arg("-C")
        .arg(&ws.dir)
        .arg("build")
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if frostbuild_core::journal::Journal::load(&ws.dir)
            .actions
            .contains_key("genrule:fast@debug")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(frostbuild_core::journal::Journal::load(&ws.dir)
        .actions
        .contains_key("genrule:fast@debug"));
    child.kill().unwrap();
    let _ = child.wait();
    let (ok, out) = ws.frost(&["plan"]);
    assert!(ok, "{out}");
    assert!(
        !out.contains("would run genrule:fast"),
        "completed action was lost:\n{out}"
    );
    assert!(out.contains("would run genrule:slow"), "{out}");
}

#[test]
fn sandbox_rejects_undeclared_workspace_header() {
    if !Path::new("/usr/bin/bwrap").exists() {
        return;
    }
    let ws = Workspace::new("sandbox");
    ws.write("secret.h", "#define SECRET 0\n");
    ws.write(
        "src/sandbox.c",
        "#include \"../secret.h\"\nint main(void) { return SECRET; }\n",
    );
    let mut manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    manifest.push_str("\n[target.sandbox_app]\nkind = \"cc_binary\"\nsrcs = [\"src/sandbox.c\"]\n");
    ws.write("frost.toml", &manifest);
    let (ok, out) = ws.frost(&["build", "sandbox_app"]);
    assert!(ok, "non-sandbox control build failed:\n{out}");
    let (ok, out) = ws.frost(&["clean", "--cache"]);
    assert!(ok, "{out}");
    let (ok, out) = ws.frost(&["build", "sandbox_app", "--sandbox"]);
    assert!(
        !ok && out.contains("secret.h"),
        "undeclared header was not diagnosed:\n{out}"
    );
}

#[test]
fn strategies_are_selectable_and_measured() {
    let ws = Workspace::new("strategies");
    for (scheduler, estimator) in [
        ("critical-path", "journal"),
        ("critical-path", "learned"),
        ("fifo", "static"),
        ("fifo", "heuristic"),
    ] {
        let dir = ws.dir.join(".frost");
        let _ = std::fs::remove_dir_all(&dir);
        let (ok, out) = ws.frost(&[
            "build",
            "--scheduler",
            scheduler,
            "--estimator",
            estimator,
            "--stats",
        ]);
        assert!(ok, "{scheduler}/{estimator} failed:\n{out}");
        // Every strategy runs the same actions and reports what it cost, so a
        // comparison never depends on rerunning with a stopwatch.
        assert!(out.contains("5 built"), "{out}");
        assert!(
            out.contains(&format!("strategy    {scheduler} / {estimator}")),
            "stats must name the strategy in effect:\n{out}"
        );
        assert!(out.contains("utilization"), "{out}");
        assert!(out.contains("critical"), "{out}");
    }
}

#[test]
fn action_reading_stdin_does_not_hang_the_build() {
    let ws = Workspace::new("stdin");
    // `cat` with no operand reads stdin. If actions inherit the terminal this
    // blocks forever and the build looks slow rather than broken.
    ws.append(
        "frost.toml",
        "\n[target.reads_stdin]\nkind = \"genrule\"\n\
         cmd = \"cat > ${out}\"\noutputs = [\"gen/stdin.txt\"]\n",
    );
    let (ok, out) = ws.frost(&["build", "reads_stdin"]);
    assert!(ok, "build must finish rather than block on stdin:\n{out}");
    assert_eq!(
        std::fs::read_to_string(ws.dir.join("gen/stdin.txt")).unwrap(),
        "",
        "stdin is empty, so the action produces an empty file"
    );
}

#[test]
fn simulate_compares_strategies_without_building() {
    let ws = Workspace::new("simulate");
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    let before = std::fs::read(ws.dir.join(".frost/journal.bin")).unwrap();

    let (ok, out) = ws.frost(&["simulate", "--jobs", "1,4"]);
    assert!(ok, "{out}");
    assert!(out.contains("critical path"), "{out}");
    assert!(out.contains("critical-path / journal"), "{out}");
    assert!(out.contains("fifo / journal"), "{out}");
    assert!(out.contains("-j 4"), "{out}");
    assert!(out.contains("fastest:"), "{out}");

    // Simulation must not touch build state: it is safe to run mid-session.
    assert_eq!(
        std::fs::read(ws.dir.join(".frost/journal.bin")).unwrap(),
        before,
        "simulate must not write to the journal"
    );

    let (ok, json) = ws.frost(&["simulate", "--json"]);
    assert!(ok, "{json}");
    let parsed: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(parsed["actions"], 5);
    let points = parsed["points"].as_array().unwrap();
    assert!(!points.is_empty());
    let cp = parsed["critical_path_ms"].as_u64().unwrap();
    for p in points {
        assert!(
            p["makespan_ms"].as_u64().unwrap() >= cp,
            "no schedule beats the critical path: {p}"
        );
    }
}

#[test]
fn a_path_is_stat_checked_once_per_build() {
    let ws = Workspace::new("stat-once");
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");

    // The generated header is gen_config's output and app's order-only input.
    // Both checks run in the same build; the second must reuse the first's
    // result rather than stat the file again.
    let (ok, out) = ws.build_explain();
    assert!(ok && out.contains("up to date"), "{out}");

    // The saving must not cost correctness: a change between builds is still
    // seen, because each build starts from a fresh cache.
    ws.write(
        "tools/gen_config.sh",
        "#!/bin/sh\nset -eu\ncat > \"$1\" <<'EOF'\n\
         #ifndef FROST_SAMPLE_CONFIG_H\n\
         #define FROST_SAMPLE_CONFIG_H\n\
         #define FROST_GREETING \"frosty:\"\n\
         #endif\nEOF\n",
    );
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    assert!(out.contains("ran genrule:gen_config"), "{out}");
}

#[test]
#[cfg(unix)]
fn daemon_works_from_a_deeply_nested_workspace() {
    // A Unix socket address is capped near 100 bytes. Keeping the socket in
    // the workspace made the daemon unusable a few directories deep, and the
    // failure surfaced as `SUN_LEN` with no mention of paths.
    let ws = Workspace::new("deep");
    // Nest outside the source workspace, or the copy recurses into itself.
    let deep = std::env::temp_dir()
        .join(format!("frost-nested-root-{}", std::process::id()))
        .join("a-directory-with-a-fairly-long-name")
        .join("and-another-level-here-as-well")
        .join("plus-a-third-level-for-good-measure")
        .join("and-a-fourth-one-to-be-quite-sure")
        .join("finally-the-workspace-itself");
    let _ = std::fs::remove_dir_all(deep.ancestors().nth(5).unwrap());
    std::fs::create_dir_all(&deep).unwrap();
    copy_dir(&ws.dir, &deep).unwrap();
    assert!(
        deep.to_string_lossy().len() > 100,
        "the test is pointless unless the path exceeds the socket limit"
    );

    let frost = |args: &[&str]| {
        let out = Command::new(frost_bin())
            .arg("-C")
            .arg(&deep)
            .args(args)
            .output()
            .expect("spawn frost");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr),
        )
    };

    let (ok, out) = frost(&["daemon", "start"]);
    assert!(ok, "daemon must start from a nested workspace:\n{out}");
    let (ok, out) = frost(&["build", "--daemon"]);
    assert!(ok, "build through the daemon failed:\n{out}");
    assert!(out.contains("5 built"), "{out}");
    let (ok, out) = frost(&["daemon", "stop"]);
    assert!(ok, "{out}");
    let _ = std::fs::remove_dir_all(deep.ancestors().nth(5).unwrap());
}

#[test]
fn include_path_environment_selects_a_different_header_and_is_keyed() {
    // CPATH changes which header the compiler finds without touching the
    // command line or any declared input. The depfile records the header that
    // was resolved *last* time, so re-digesting it proves nothing: unless the
    // environment is part of the action key, frost reports everything cached
    // and hands back a binary built against the other header.
    let ws = Workspace::new("cpath");
    let one = ws.dir.join("inc-one");
    let two = ws.dir.join("inc-two");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(one.join("tuning.h"), "#define TUNING 1\n").unwrap();
    std::fs::write(two.join("tuning.h"), "#define TUNING 99\n").unwrap();

    let util = std::fs::read_to_string(ws.dir.join("src/util.c")).unwrap();
    ws.write(
        "src/util.c",
        &format!(
            "#include <tuning.h>\n{}",
            util.replace(
                "return a + b + FROST_INTERNAL_BIAS;",
                "return a + b + FROST_INTERNAL_BIAS + TUNING;"
            )
        ),
    );

    let run_with = |dir: &std::path::Path| {
        let (ok, out) = ws.frost_env(&["build"], &[("CPATH", dir.to_str().unwrap())]);
        assert!(ok, "build failed:\n{out}");
        let app = Command::new(ws.dir.join(".frost/bin/debug/app"))
            .output()
            .expect("run built app");
        (out, String::from_utf8_lossy(&app.stdout).to_string())
    };

    let (_, first) = run_with(&one);
    assert_eq!(first, "frost: 43\n");

    let (out, second) = run_with(&two);
    assert_eq!(
        second, "frost: 141\n",
        "a different header must produce a different binary:\n{out}"
    );
    assert!(
        !out.contains("up to date"),
        "the environment change must invalidate, not report everything cached:\n{out}"
    );

    let (_, back) = run_with(&one);
    assert_eq!(back, "frost: 43\n", "switching back is equally observable");
}

#[test]
fn a_glob_that_matches_nothing_is_reported_where_it_is_written() {
    // A typo in a srcs glob used to produce an empty archive that built
    // happily, and the build then failed at the link with a message about
    // symbols — nowhere near the cause.
    let ws = Workspace::new("empty-glob");
    let good = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    let (ok, out) = ws.frost(&["build"]);
    assert!(
        ok,
        "the workspace builds before the typo is introduced:\n{out}"
    );

    ws.append(
        "frost.toml",
        "\n[target.typo]\nkind = \"cc_library\"\nsrcs = [\"srcs/**/*.c\"]\n",
    );
    let (ok, out) = ws.frost(&["build", "typo"]);
    assert!(!ok, "an empty glob must not build:\n{out}");
    assert!(out.contains("matched no files"), "{out}");
    assert!(out.contains("typo"), "the target must be named:\n{out}");

    // The manifest is rejected as a whole, so an unrelated target cannot be
    // built around it either — a broken manifest is broken for everyone.
    let (ok, out) = ws.frost(&["build", "util"]);
    assert!(!ok, "{out}");
    assert!(out.contains("matched no files"), "{out}");

    ws.write("frost.toml", &good);
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "removing the typo restores the build:\n{out}");
}

#[test]
fn init_writes_a_manifest_that_actually_builds() {
    // Running frost in a directory with sources but no manifest used to end
    // at an error with no next step. The scaffold has to be good enough to
    // build as written, or it is just a different dead end.
    let dir = std::env::temp_dir().join(format!("frost-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("include")).unwrap();
    std::fs::write(
        dir.join("src/main.c"),
        "#include <stdio.h>\n#include \"util.h\"\n\
         int main(void) { printf(\"%d\\n\", add(20, 22)); return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/util.c"),
        "#include \"util.h\"\nint add(int a, int b) { return a + b; }\n",
    )
    .unwrap();
    std::fs::write(dir.join("include/util.h"), "int add(int, int);\n").unwrap();

    let frost = |args: &[&str]| {
        let out = Command::new(frost_bin())
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .expect("spawn frost");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr),
        )
    };

    let (ok, out) = frost(&["build"]);
    assert!(!ok);
    assert!(
        out.contains("frost init"),
        "the error must name a next step:\n{out}"
    );

    let (ok, out) = frost(&["init"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("src/main.c"),
        "the summary names the entry point:\n{out}"
    );

    let (ok, out) = frost(&["build"]);
    assert!(ok, "the scaffold must build as written:\n{out}");
    let run = Command::new(dir.join(".frost/bin/debug").join(dir.file_name().unwrap()))
        .output()
        .expect("run built binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");

    // init refuses to clobber an existing manifest, and says how to look
    // without writing.
    let (ok, out) = frost(&["init"]);
    assert!(!ok, "{out}");
    assert!(out.contains("--dry-run"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_refuses_a_directory_it_cannot_describe() {
    let dir = std::env::temp_dir().join(format!("frost-init-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.ts"), "export const x = 1;\n").unwrap();

    let out = Command::new(frost_bin())
        .arg("-C")
        .arg(&dir)
        .arg("init")
        .output()
        .expect("spawn frost");
    assert!(
        !out.status.success(),
        "frost builds C and C++, not TypeScript"
    );
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("no C or C++ sources"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[cfg(unix)]
fn a_mode_change_invalidates_and_a_restored_output_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    // `chmod -x` on a script a genrule runs leaves every byte in place. With
    // a content-only digest frost reported the build as current while a clean
    // build of the same tree failed — the cache disagreeing with the source.
    let ws = Workspace::new("mode");
    ws.write("tools/run.sh", "#!/bin/sh\nprintf 'ran\\n' > \"$1\"\n");
    std::fs::set_permissions(
        ws.dir.join("tools/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    ws.append(
        "frost.toml",
        "\n[target.viashell]\nkind = \"genrule\"\ncmd = \"./tools/run.sh ${out}\"\n\
         inputs = [\"tools/run.sh\"]\noutputs = [\"gen/ran.txt\"]\n",
    );

    let (ok, out) = ws.frost(&["build", "viashell"]);
    assert!(ok, "{out}");

    std::fs::set_permissions(
        ws.dir.join("tools/run.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let (ok, out) = ws.frost(&["build", "viashell"]);
    assert!(
        !ok,
        "a build that a clean tree cannot reproduce must not report success:\n{out}"
    );
    assert!(!out.contains("up to date"), "{out}");

    // Restoring the bit restores the build.
    std::fs::set_permissions(
        ws.dir.join("tools/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (ok, out) = ws.frost(&["build", "viashell"]);
    assert!(ok, "{out}");

    // An executable output restored from the CAS has to come back executable,
    // or the next action that runs it fails.
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "{out}");
    let app = ws.dir.join(".frost/bin/debug/app");
    let before = std::fs::metadata(&app).unwrap().permissions().mode();
    assert!(before & 0o111 != 0, "the built binary is executable");
    std::fs::remove_file(&app).unwrap();
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::metadata(&app).unwrap().permissions().mode() & 0o111,
        before & 0o111,
        "a binary restored from the CAS must still be executable"
    );
    assert_eq!(ws.run_app(), "frost: 42\n");
}

#[test]
#[cfg(unix)]
fn a_different_toolchain_binary_invalidates_the_workspace() {
    // The fingerprint covers the resolved driver binaries, so pointing the
    // manifest at a different one has to invalidate even though no source
    // changed. (That the shell is in the same set is asserted in
    // frostbuild-exec, where the stamp can be read directly — swapping the
    // machine's /bin/sh is not something a test should do.)
    let ws = Workspace::new("shell");
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    let (ok, out) = ws.build_explain();
    assert!(ok && out.contains("up to date"), "{out}");

    // Point the workspace at a private copy of the shell, so the fingerprint
    // has something to notice without touching the machine's /bin/sh.
    let fake = ws.dir.join("fake-cc");
    std::fs::copy("/bin/sh", &fake).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    ws.write(
        "frost.toml",
        &std::fs::read_to_string(ws.dir.join("frost.toml"))
            .unwrap()
            .replace("cc = \"cc\"", &format!("cc = {:?}", fake.to_str().unwrap())),
    );
    let (_, out) = ws.build_explain();
    assert!(
        !out.contains("up to date"),
        "a different C driver must invalidate:\n{out}"
    );
}

#[test]
fn a_corrupt_cas_object_is_rebuilt_rather_than_handed_back() {
    // The CAS is content-addressed: an object's name is its digest. An object
    // that no longer hashes to its own name is corrupt, and restoring it
    // would deliver an artifact that never existed while reporting the build
    // as current — the worst failure a build system has.
    let ws = Workspace::new("cas-corrupt");
    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    let app = ws.dir.join(".frost/bin/debug/app");
    let correct = std::fs::read(&app).unwrap();

    // Find the object backing the built binary and flip one byte, keeping the
    // size identical so nothing but a content check can notice.
    let mut objects = Vec::new();
    let mut stack = vec![ws.dir.join(".frost/cas")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                objects.push(path);
            }
        }
    }
    let object = objects
        .iter()
        .find(|p| std::fs::read(p).is_ok_and(|b| b == correct))
        .expect("the built binary is in the CAS");
    let mut bytes = std::fs::read(object).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    std::fs::write(object, &bytes).unwrap();
    std::fs::remove_file(&app).unwrap();

    let (ok, out) = ws.build_explain();
    assert!(ok, "{out}");
    assert!(
        !out.contains("up to date"),
        "a corrupt object must not be restored as if current:\n{out}"
    );
    assert_eq!(
        std::fs::read(&app).unwrap(),
        correct,
        "the action is re-run, so the output matches a correct build"
    );
    assert_eq!(ws.run_app(), "frost: 42\n");
}
