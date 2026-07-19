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
        let dir = std::env::temp_dir().join(format!("frost-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&src, &dir).expect("copy sample_c");
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
        out.contains("5 executed"),
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
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");
    let (ok, out) = ws.build_explain();
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");
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
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");
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
    assert!(
        out.contains("5 executed, 0 cached"),
        "unexpected summary:\n{out}"
    );
    assert_eq!(ws.run_app(), "frost: 42\n");

    let (ok, out) = ws.build_explain();
    assert!(ok, "no-op build failed:\n{out}");
    assert!(
        out.contains("0 executed, 5 cached"),
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
        out.contains("1 executed, 4 cached"),
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
    assert!(
        out.contains("0 executed, 5 cached"),
        "CAS restore expected:\n{out}"
    );
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
        out.contains("5 cached"),
        "CAS should restore outputs:\n{out}"
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
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");
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
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");

    // The daemon must not trust only watcher state: build outputs live under
    // .frost (which the watcher intentionally ignores). The engine still has
    // to validate outputs and restore a manually deleted artifact from CAS.
    std::fs::remove_file(ws.dir.join(".frost/bin/debug/app")).unwrap();
    let (ok, out) = ws.frost(&["build", "--daemon", "--explain"]);
    assert!(ok && out.contains("0 executed, 5 cached"), "{out}");
    assert_eq!(ws.run_app(), "frost: 42\n");

    ws.append("src/util.c", "\n/* daemon watcher change */\n");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let (ok, out) = ws.frost(&["build", "--daemon"]);
    assert!(
        ok && out.contains("1 executed"),
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
