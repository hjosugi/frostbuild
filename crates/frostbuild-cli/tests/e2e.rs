//! End-to-end tests driving the real `frost` binary against the sample_c
//! workspace with the host C compiler.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;

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

    #[cfg(unix)]
    fn frost_pty(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let command_line = pty_command_line(&self.dir, args);
        let mut command = Command::new("script");
        command
            // `-- <command> <args...>` was added to newer util-linux
            // releases. Ubuntu's CI image still requires the long-standing
            // `-c <command>` form.
            .args(["-q", "-e", "-c"])
            .arg(command_line)
            .arg("/dev/null");
        if !env.iter().any(|(key, _)| *key == "CI") {
            // GitHub Actions sets CI for the test harness itself. Positive TTY
            // cases must model an interactive user, while the dedicated CI
            // case below explicitly puts it back.
            command.env_remove("CI");
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let out = command.output().expect("spawn frost in a pseudo-terminal");
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

#[test]
fn host_portable_command_target_builds_and_caches() {
    let ws = Workspace::empty("host-command");
    #[cfg(unix)]
    let (shell, shell_arg, command) = ("/bin/sh", "-c", "printf host-ok > ${config}/host.txt");
    #[cfg(windows)]
    let (shell, shell_arg, command) = ("cmd.exe", "/C", "echo host-ok>${config}/host.txt");
    ws.write(
        "frost.toml",
        &format!(
            r#"[workspace]
default_targets = ["smoke"]

[toolchain]
cc = "{shell}"
cxx = "{shell}"
ar = "{shell}"

[toolchain.tools]
host = "{shell}"

[target.smoke]
kind = "command"
tool = "host"
args = ["{shell_arg}", "{command}"]
outputs = ["${{config}}/host.txt"]
"#
        ),
    );

    let (ok, out) = ws.frost(&["build"]);
    assert!(
        ok && out.contains("1 built"),
        "portable build failed:\n{out}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.dir.join("debug/host.txt"))
            .unwrap()
            .trim(),
        "host-ok"
    );

    let (ok, out) = ws.frost(&["build"]);
    assert!(
        ok && (out.contains("1 cached") || out.contains("up to date")),
        "portable no-op failed:\n{out}"
    );
    let (ok, out) = ws.frost(&["cache", "stats", "--json"]);
    assert!(ok, "cache stats failed:\n{out}");
    let stats: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stats["object_count"], 1);
    assert!(stats["chunk_reuse_ratio"].is_number());
}

#[cfg(unix)]
fn pty_command_line(workspace: &Path, args: &[&str]) -> String {
    let command = std::iter::once(frost_bin().to_string())
        .chain(std::iter::once("-C".to_string()))
        .chain(std::iter::once(workspace.to_string_lossy().into_owned()))
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .map(|arg| shell_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    // `script -c` starts a shell on older util-linux. Replacing that shell
    // keeps Frost as the foreground process that receives raw-mode Ctrl-C.
    format!("exec {command}")
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
fn piped_build_uses_stable_plain_progress() {
    let ws = Workspace::new("plain-progress");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("[1/"),
        "plain progress was not printed:\n{out}"
    );
    assert!(
        !out.contains("\u{1b}["),
        "piped output must not contain terminal control codes:\n{out:?}"
    );
}

#[test]
#[cfg(unix)]
fn tty_build_shows_live_slots_cache_critical_path_and_logs() {
    let ws = Workspace::new("tui-progress");
    let (ok, out) = ws.frost_pty(&["build"], &[]);
    assert!(ok, "{out}");
    assert!(
        out.contains("\u{1b}[?1049h"),
        "TTY build did not enter the live screen:\n{out:?}"
    );
    for label in ["slots", "cache", "critical path:", "logs ("] {
        assert!(out.contains(label), "TUI omitted {label:?}:\n{out:?}");
    }

    let (ok, cached) = ws.frost_pty(&["build"], &[]);
    assert!(ok, "{cached}");
    assert!(
        cached.contains("cache  5 hit"),
        "live cache-hit state was not updated:\n{cached:?}"
    );
    assert!(cached.contains("up to date"), "{cached}");
}

#[test]
#[cfg(unix)]
fn no_tui_and_ci_force_plain_output_even_on_a_tty() {
    for (name, args, env) in [
        ("no-tui", vec!["build", "--no-tui"], vec![]),
        ("ci", vec!["build"], vec![("CI", "1")]),
    ] {
        let ws = Workspace::new(name);
        let (ok, out) = ws.frost_pty(&args, &env);
        assert!(ok, "{out}");
        assert!(
            out.contains("[1/"),
            "plain progress was not printed:\n{out}"
        );
        assert!(
            !out.contains("\u{1b}[?1049h"),
            "{name} unexpectedly enabled the live screen:\n{out:?}"
        );
    }
}

#[test]
#[cfg(unix)]
fn tty_failure_is_rendered_before_the_summary() {
    let ws = Workspace::empty("tui-failure");
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["broken"]

[target.broken]
kind = "genrule"
cmd = "printf immediate-failure >&2; exit 7"
outputs = ["broken.txt"]
"#,
    );
    let (ok, out) = ws.frost_pty(&["build"], &[]);
    assert!(!ok, "broken action unexpectedly succeeded:\n{out}");
    let immediate = out.find("FAILED:").expect("failure was not rendered");
    let summary = out
        .rfind("failure summary")
        .expect("failure summary was not printed");
    assert!(
        immediate < summary,
        "failure did not appear before the summary:\n{out:?}"
    );
}

#[test]
#[cfg(unix)]
fn ctrl_c_in_raw_tui_mode_still_cancels_the_build() {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::mpsc;

    let ws = Workspace::empty("tui-cancel");
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["slow"]

[target.slow]
kind = "genrule"
cmd = "sleep 3; printf done > ${out}"
outputs = ["slow.txt"]
"#,
    );
    let started = std::time::Instant::now();
    let mut child = Command::new("script")
        .args(["-q", "-e", "-c"])
        .arg(pty_command_line(&ws.dir, &["build"]))
        .arg("/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CI")
        .spawn()
        .expect("spawn TUI build");
    let mut stdout = child.stdout.take().expect("script stdout");
    let (ready_sender, ready_receiver) = mpsc::channel();
    let output_reader = std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut announced = false;
        loop {
            let read = stdout.read(&mut buffer).expect("read TUI output");
            if read == 0 {
                break;
            }
            captured.extend_from_slice(&buffer[..read]);
            if !announced
                && captured
                    .windows(b"\x1b[?1049h".len())
                    .any(|window| window == b"\x1b[?1049h")
            {
                let _ = ready_sender.send(());
                announced = true;
            }
        }
        captured
    });
    ready_receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("TUI did not enter raw alternate-screen mode");
    child
        .stdin
        .take()
        .expect("script stdin")
        .write_all(&[3])
        .expect("send Ctrl-C");
    let status = child.wait().expect("wait for cancelled build");
    let captured = output_reader.join().expect("join TUI output reader");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("script stderr")
        .read_to_end(&mut stderr)
        .expect("read script stderr");
    let output = String::from_utf8_lossy(&captured).to_string() + &String::from_utf8_lossy(&stderr);
    assert_eq!(status.code(), Some(130), "{output:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(2_500),
        "Ctrl-C was swallowed by raw terminal mode"
    );
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

    let (ok, out) = ws.frost(&["build", "--all-platforms", "--explain"]);
    assert!(ok, "multi-platform build failed:\n{out}");
    assert!(out.contains("multi-platform build (2 platforms"), "{out}");
    assert!(
        out.contains("|-- host") && out.contains("`-- devsim"),
        "{out}"
    );
    assert!(out.contains("platform summary"), "{out}");
}

#[test]
#[cfg(unix)]
fn command_adapter_is_platform_aware_keyed_and_language_agnostic() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::empty("command-adapter");
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();
    ws.write("source.txt", "payload\n");
    for (name, identity) in [("adapter", "host"), ("device-adapter", "device")] {
        let path = ws.dir.join("tools").join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nIFS= read -r content < \"$1\"\n\
                 printf '{}|%s|%s|%s|%s/%s\\n' \"$content\" \"$STATIC_VALUE\" \
                 \"${{LANGUAGE_FLAG-unset}}\" \"$3\" \"$4\" > \"$2\"\n",
                identity
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["artifact"]

[toolchain.tools]
adapter = "tools/adapter"

[platform.device.tools]
adapter = "tools/device-adapter"

[target.artifact]
kind = "command"
tool = "adapter"
args = ["${in}", "${out}", "${profile}", "${platform}"]
inputs = ["source.txt"]
outputs = [".frost/out/${config}/artifact.txt"]
env = { STATIC_VALUE = "manifest" }
pass_env = ["LANGUAGE_FLAG"]
sandbox = false
"#,
    );

    let build = |flag: &str, args: &[&str]| {
        let (ok, out) = ws.frost_env(args, &[("LANGUAGE_FLAG", flag)]);
        assert!(ok, "command adapter build failed:\n{out}");
        out
    };
    build("one", &["build"]);
    let warm = build("one", &["build"]);
    assert!(warm.contains("up to date"), "{warm}");
    assert!(ws.dir.join(".frost/noop-debug.bin").is_file());

    let changed = build("two", &["build"]);
    assert!(
        !changed.contains("up to date"),
        "pass_env must invalidate the action and fast no-op certificate:\n{changed}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.dir.join(".frost/out/debug/artifact.txt")).unwrap(),
        "host|payload|manifest|two|debug/host\n"
    );

    let all = build("two", &["build", "--all-platforms"]);
    assert!(all.contains("platform summary"), "{all}");
    assert_eq!(
        std::fs::read_to_string(ws.dir.join(".frost/out/device/debug/artifact.txt")).unwrap(),
        "device|payload|manifest|two|debug/device\n"
    );

    let host_tool = ws.dir.join("tools/adapter");
    let mut updated = std::fs::read_to_string(&host_tool)
        .unwrap()
        .replace("host|%s", "host-v2|%s");
    updated.push('\n');
    std::fs::write(&host_tool, updated).unwrap();
    std::fs::set_permissions(&host_tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let changed = build("two", &["build"]);
    assert!(!changed.contains("up to date"), "{changed}");
    assert!(
        std::fs::read_to_string(ws.dir.join(".frost/out/debug/artifact.txt"))
            .unwrap()
            .starts_with("host-v2|"),
        "changing a named tool must invalidate its command action"
    );

    let (ok, out) = ws.frost(&["clean"]);
    assert!(ok, "multi-configuration clean failed:\n{out}");
    assert!(!ws.dir.join(".frost/out/debug/artifact.txt").exists());
    assert!(!ws.dir.join(".frost/out/device/debug/artifact.txt").exists());
}

#[test]
#[cfg(unix)]
fn command_adapter_can_preserve_outputs_for_incremental_compilers() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::empty("preserve-command-outputs");
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();
    ws.write("source.txt", "one\n");
    ws.write(
        "tools/incremental",
        r#"#!/bin/sh
set -eu
state=$1
stable=$2
changed=$3
input=$4
IFS= read -r value < "$input"
if [ "$value" = one ]; then
  printf 'state\n' > "$state"
  printf 'stable\n' > "$stable"
else
  [ -f "$state" ] && [ -f "$stable" ] || exit 42
fi
printf '%s\n' "$value" > "$changed"
"#,
    );
    std::fs::set_permissions(
        ws.dir.join("tools/incremental"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["incremental"]

[toolchain.tools]
incremental = "tools/incremental"

[target.incremental]
kind = "command"
tool = "incremental"
args = ["${outs}", "${in}"]
inputs = ["source.txt"]
outputs = [
  ".frost/out/${config}/state.txt",
  ".frost/out/${config}/stable.txt",
  ".frost/out/${config}/changed.txt",
]
preserve_outputs = true
sandbox = false
"#,
    );

    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "initial incremental build failed:\n{out}");
    ws.write("source.txt", "two\n");
    let (ok, out) = ws.frost(&["build", "--explain"]);
    assert!(ok, "incremental rerun lost its prior outputs:\n{out}");
    assert!(out.contains("ran command:incremental"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ws.dir.join(".frost/out/debug/stable.txt")).unwrap(),
        "stable\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.dir.join(".frost/out/debug/changed.txt")).unwrap(),
        "two\n"
    );
}

#[test]
fn command_adapter_builds_real_rust_go_java_python_and_typescript_tools() {
    let ws = Workspace::empty("language-tools");
    std::fs::create_dir_all(ws.dir.join("src")).unwrap();
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();

    let available = |tool: &str| Command::new(tool).arg("--version").output().is_ok();
    let mut tools = Vec::new();
    let mut targets = Vec::new();
    let mut defaults = Vec::new();

    if available("rustc") {
        ws.write("src/main.rs", "fn main() { println!(\"rust-ok\"); }\n");
        tools.push("rustc = \"rustc\"".to_string());
        targets.push(
            r#"[target.rust]
kind = "command"
tool = "rustc"
args = ["${in}", "-o", "${out}"]
inputs = ["src/main.rs"]
outputs = [".frost/out/${config}/rust-app"]
sandbox = false
"#
            .to_string(),
        );
        defaults.push("rust");
    }
    if available("go") {
        ws.write(
            "src/main.go",
            "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"go-ok\") }\n",
        );
        tools.push("go = \"go\"".to_string());
        targets.push(
            r#"[target.go]
kind = "command"
tool = "go"
args = ["build", "-o", "${out}", "${in}"]
inputs = ["src/main.go"]
outputs = [".frost/out/${config}/go-app"]
sandbox = false
"#
            .to_string(),
        );
        defaults.push("go");
    }
    if available("javac") {
        ws.write(
            "src/Hello.java",
            "public final class Hello { static final class Nested {} \
             public static void main(String[] a) { System.out.println(\"java-ok\"); } }\n",
        );
        tools.push("javac = \"javac\"".to_string());
        tools.push(format!(
            "pack_jar = {}",
            serde_json::to_string(frost_bin()).unwrap()
        ));
        targets.push(
            r#"[target.java]
kind = "command"
tool = "javac"
args = ["-d", "${clean_dir}", "${in}"]
inputs = ["src/Hello.java"]
outputs = [".frost/out/${config}/java.jar"]
clean_dirs = [".frost/tmp/${config}/java"]
steps = [{ tool = "pack_jar", args = ["pack-jar", "--input", "${clean_dir}",
                                       "--output", "${out}",
                                       "--main-class", "Hello"] }]
sandbox = false
"#
            .to_string(),
        );
        defaults.push("java");
    }
    if available("python3") {
        std::fs::create_dir_all(ws.dir.join("src/frost_language_demo")).unwrap();
        ws.write(
            "src/frost_language_demo/__init__.py",
            "def message():\n    return 'python-ok'\n",
        );
        tools.push(format!(
            "pack_wheel = {}",
            serde_json::to_string(frost_bin()).unwrap()
        ));
        targets.push(
            r#"[target.python]
kind = "command"
tool = "pack_wheel"
args = ["pack-wheel", "--input", "src", "--distribution", "frost-language-demo",
        "--version", "1.0.0", "--output", "${out}"]
inputs = ["src/frost_language_demo/__init__.py"]
outputs = [".frost/out/${config}/frost_language_demo-1.0.0-py3-none-any.whl"]
sandbox = false
"#
            .to_string(),
        );
        defaults.push("python");
    }

    // Modern Node runs erasable TypeScript syntax directly. Probe first so
    // older CI images simply exercise the other real adapters.
    if available("node") {
        ws.write(
            "src/write.ts",
            "import { writeFileSync } from 'node:fs';\n\
             const message: string = 'typescript-ok\\n';\n\
             writeFileSync(process.argv[2], message);\n",
        );
        let probe = Command::new("node")
            .arg(ws.dir.join("src/write.ts"))
            .arg(ws.dir.join("typescript-probe.txt"))
            .output();
        if probe.is_ok_and(|output| output.status.success()) {
            tools.push("node = \"node\"".to_string());
            targets.push(
                r#"[target.typescript]
kind = "command"
tool = "node"
args = ["src/write.ts", "${out}"]
inputs = ["src/write.ts"]
outputs = [".frost/out/${config}/typescript.txt"]
sandbox = false
"#
                .to_string(),
            );
            defaults.push("typescript");
        }
    }

    assert!(
        !defaults.is_empty(),
        "the test host has no supported language tool"
    );
    ws.write(
        "frost.toml",
        &format!(
            "[workspace]\ndefault_targets = [{}]\n\n[toolchain.tools]\n{}\n\n{}",
            defaults
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            tools.join("\n"),
            targets.join("\n")
        ),
    );

    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "real language adapter build failed:\n{out}");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok && out.contains("up to date"), "{out}");
    if defaults.contains(&"rust") {
        assert!(ws.dir.join(".frost/out/debug/rust-app").is_file());
    }
    if defaults.contains(&"go") {
        assert!(ws.dir.join(".frost/out/debug/go-app").is_file());
    }
    if defaults.contains(&"java") {
        let archive = ws.dir.join(".frost/out/debug/java.jar");
        assert!(archive.is_file());
        let listing = || {
            let file = std::fs::File::open(&archive).unwrap();
            let mut jar = zip::ZipArchive::new(file).unwrap();
            (0..jar.len())
                .map(|index| jar.by_index(index).unwrap().name().to_string())
                .collect::<Vec<_>>()
        };
        let entries = listing();
        assert!(
            entries.iter().any(|name| name == "Hello.class"),
            "{entries:?}"
        );
        assert!(
            entries.iter().any(|name| name == "Hello$Nested.class"),
            "{entries:?}"
        );
        if available("java") {
            let output = Command::new("java")
                .arg("-jar")
                .arg(&archive)
                .output()
                .expect("run Frost-packed Java archive");
            assert!(output.status.success(), "{output:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "java-ok\n");
        }
        #[cfg(unix)]
        {
            let (ok, out) = ws.frost(&[
                "debug",
                "java",
                "--debugger",
                "/bin/echo",
                "--print",
                "--",
                "argument",
            ]);
            assert!(ok, "Java debug argv generation failed:\n{out}");
            assert!(out.contains("Java/jdb"), "{out}");
            assert!(out.contains("-classpath"), "{out}");
            assert!(out.contains("Hello"), "{out}");
        }

        // The intermediate tree is reset before the next multi-step action.
        // Removing a nested class must not leave stale bytecode in the jar.
        ws.write(
            "src/Hello.java",
            "public final class Hello { public static void main(String[] a) { \
             System.out.println(\"java-ok-v2\"); } }\n",
        );
        let (ok, out) = ws.frost(&["build"]);
        assert!(ok, "Java multi-step rebuild failed:\n{out}");
        let entries = listing();
        assert!(
            entries.iter().any(|name| name == "Hello.class"),
            "{entries:?}"
        );
        assert!(
            entries.iter().all(|name| name != "Hello$Nested.class"),
            "{entries:?}"
        );
    }
    if defaults.contains(&"python") {
        let wheel = ws
            .dir
            .join(".frost/out/debug/frost_language_demo-1.0.0-py3-none-any.whl");
        let output = Command::new("python3")
            .args([
                "-c",
                "import sys; sys.path.insert(0, sys.argv[1]); \
                 import frost_language_demo; print(frost_language_demo.message())",
            ])
            .arg(&wheel)
            .output()
            .expect("run Frost-packed Python wheel");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "python-ok\n");
    }
    if defaults.contains(&"typescript") {
        assert_eq!(
            std::fs::read_to_string(ws.dir.join(".frost/out/debug/typescript.txt")).unwrap(),
            "typescript-ok\n"
        );
    }
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
fn completion_scripts_and_fzf_selection_are_available() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::new("completion");
    for (shell, marker) in [
        ("bash", "_frost"),
        ("zsh", "#compdef frost"),
        ("fish", "__fish_frost"),
        ("powershell", "Register-ArgumentCompleter"),
        ("elvish", "arg-completer[frost]"),
        ("nushell", "export extern frost"),
    ] {
        let (ok, out) = ws.frost(&["completions", shell]);
        assert!(ok && out.contains(marker), "{shell} completion:\n{out}");
        assert!(
            out.contains("pack-jar"),
            "{shell} completion omitted pack-jar:\n{out}"
        );
        assert!(
            out.contains("pack-wheel"),
            "{shell} completion omitted pack-wheel:\n{out}"
        );
        assert!(
            out.contains("bazel-dev"),
            "{shell} completion omitted bazel-dev:\n{out}"
        );
        for command in ["dev", "debug", "ide", "doctor", "cache", "init", "language"] {
            assert!(
                out.contains(command),
                "{shell} completion omitted {command}:\n{out}"
            );
        }
    }

    let dynamic = |words: &[&str], index: usize| {
        let out = Command::new(frost_bin())
            .arg("--")
            .args(words)
            .env("COMPLETE", "bash")
            .env("_CLAP_IFS", "\u{b}")
            .env("_CLAP_COMPLETE_INDEX", index.to_string())
            .env("_CLAP_COMPLETE_COMP_TYPE", "9")
            .env("_CLAP_COMPLETE_SPACE", "true")
            .output()
            .expect("query dynamic completion");
        assert!(out.status.success(), "dynamic completion failed: {out:?}");
        String::from_utf8(out.stdout)
            .unwrap()
            .split('\u{b}')
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let root = ws.dir.to_str().unwrap();
    let targets = dynamic(&["frost", "-C", root, "build", ""], 4);
    for target in ["app", "gen_config", "util"] {
        assert!(targets.contains(&target.to_string()), "{targets:?}");
    }
    assert_eq!(
        dynamic(&["frost", "-C", root, "build", "--profile", ""], 5),
        ["debug"]
    );
    assert_eq!(
        dynamic(&["frost", "-C", root, "build", "--platform", ""], 5),
        ["host"]
    );
    assert_eq!(
        dynamic(&["frost", "init", "--language", ""], 3),
        ["native", "java"]
    );

    #[cfg(unix)]
    {
        let tools = ws.dir.join("completion-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let fzf = tools.join("fzf");
        std::fs::write(
            &fzf,
            "#!/bin/sh\nIFS= read -r selected\nprintf '%s\\n' \"$selected\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fzf, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (ok, out) = ws.frost_env(&["pick", "--print"], &[("PATH", tools.to_str().unwrap())]);
        assert!(ok, "fzf-backed selection failed:\n{out}");
        assert_eq!(out.trim(), "app");
    }
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
fn plain_default_build_uses_and_invalidates_the_fast_noop_certificate() {
    let ws = Workspace::new("fast-noop");

    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "clean build failed:\n{out}");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok && out.contains("up to date"), "{out}");
    assert!(
        ws.dir.join(".frost/noop-debug.bin").is_file(),
        "the fully checked no-op did not persist its certificate"
    );

    // A fast hit does not need to reconstruct the per-action journal.
    std::fs::write(ws.dir.join(".frost/journal.bin"), b"corrupt journal").unwrap();
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok && out.contains("up to date"), "{out}");

    // Corrupting the shortcut itself is a cache miss, never a build failure
    // and never evidence that stale outputs are current.
    let certificate = ws.dir.join(".frost/noop-debug.bin");
    let mut corrupt = std::fs::read(&certificate).unwrap();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x80;
    std::fs::write(&certificate, corrupt).unwrap();
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "corrupt certificate did not fall back safely:\n{out}");

    // The certificate is only a shortcut: any input mismatch falls back to
    // the normal path. With the journal deliberately unusable, that path
    // rebuilds the closure rather than accepting the stale certificate.
    ws.write(
        "src/util_internal.h",
        "#ifndef FROST_SAMPLE_UTIL_INTERNAL_H\n\
         #define FROST_SAMPLE_UTIL_INTERNAL_H\n\
         #define FROST_INTERNAL_BIAS 1\n\
         #endif\n",
    );
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "fallback build failed:\n{out}");
    assert!(!out.contains("up to date"), "{out}");
    assert_eq!(ws.run_app(), "frost: 43\n");
}

#[test]
fn fast_noop_certificate_is_bound_to_the_default_target_graph() {
    let ws = Workspace::new("fast-noop-graph");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "{out}");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok && out.contains("up to date"), "{out}");
    let certificate = ws.dir.join(".frost/noop-debug.bin");
    let old_certificate = std::fs::read(&certificate).unwrap();

    let manifest = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    ws.write(
        "frost.toml",
        &format!(
            "{}\n\
             [target.extra]\n\
             kind = \"genrule\"\n\
             cmd = \"printf extra > ${{out}}\"\n\
             outputs = [\".frost/extra/result.txt\"]\n",
            manifest.replace(
                "default_targets = [\"app\"]",
                "default_targets = [\"app\", \"extra\"]"
            )
        ),
    );
    let extra = ws.dir.join(".frost/extra/result.txt");
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "new default target failed:\n{out}");
    assert!(extra.is_file(), "{out}");
    assert_eq!(
        std::fs::read(&certificate).unwrap(),
        old_certificate,
        "a build that executed work should leave the prior certificate in place"
    );

    std::fs::remove_file(&extra).unwrap();
    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "stale graph certificate blocked fallback:\n{out}");
    assert!(
        extra.is_file(),
        "the old default-target certificate skipped the new target:\n{out}"
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
fn direct_argv_language_test_is_cached_and_cleans_a_failed_stamp() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping direct_argv_language_test: python3 not in PATH");
        return;
    }
    let ws = Workspace::new("direct-test");
    std::fs::create_dir_all(ws.dir.join("tests")).unwrap();
    ws.write("tests/value.txt", "pass\n");
    ws.write(
        "tests/check.py",
        concat!(
            "import os, pathlib, sys\n",
            "actual = pathlib.Path(sys.argv[1]).read_text().strip()\n",
            "expected = os.environ['EXPECTED']\n",
            "if actual != expected:\n",
            "    raise SystemExit(f'expected {expected!r}, got {actual!r}')\n",
        ),
    );
    ws.append(
        "frost.toml",
        r#"
[toolchain.tools]
python = "python3"

[target.python_test]
kind = "test"
tool = "python"
args = ["tests/check.py", "tests/value.txt"]
inputs = ["tests/check.py", "tests/value.txt"]
env = { EXPECTED = "pass" }
sandbox = false
"#,
    );

    let (ok, out) = ws.frost(&["test", "python_test", "--explain"]);
    assert!(ok && out.contains("tests: 1 passed"), "{out}");
    let stamp = ws.dir.join(".frost/test/debug/python_test/passed");
    assert!(stamp.is_file());
    let (ok, out) = ws.frost(&["test", "python_test"]);
    assert!(ok && out.contains("1 cached"), "{out}");

    ws.write("tests/value.txt", "fail\n");
    let (ok, out) = ws.frost(&["test", "python_test", "--explain"]);
    assert!(!ok, "changed failing test must run and fail");
    assert!(out.contains("expected 'pass', got 'fail'"), "{out}");
    assert!(
        !stamp.exists(),
        "a failed test must not retain its success stamp"
    );
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

    // A valid certificate must be answered inside frostd. The deliberately
    // nonexistent fallback program proves that no second frost process was
    // needed for this hit.
    let in_process = frostbuild_daemon::request(
        &ws.dir,
        &frostbuild_daemon::Request::Run {
            version: frostbuild_daemon::PROTOCOL_VERSION,
            program: ws.dir.join("definitely-missing-frost"),
            args: Vec::new(),
            fast_noop: Some(frostbuild_daemon::FastNoopRequest {
                profile: "debug".into(),
                platform: frostbuild_core::manifest::HOST_PLATFORM.into(),
                key_env: frostbuild_exec::key_environment_snapshot(),
            }),
        },
    )
    .unwrap();
    assert_eq!(in_process.code, 0, "{in_process:?}");
    assert!(in_process.stdout.contains("up to date"), "{in_process:?}");

    // A watcher barrier must observe output changes under .frost before a
    // cached proof can be accepted. Deleting an artifact immediately before
    // the request must take the full path and restore it from CAS.
    std::fs::remove_file(ws.dir.join(".frost/bin/debug/app")).unwrap();
    let (ok, out) = ws.frost(&["build", "--daemon", "--explain"]);
    assert!(ok && out.contains("up to date"), "{out}");
    assert_eq!(ws.run_app(), "frost: 42\n");

    ws.append("src/util.c", "\n/* daemon watcher change */\n");
    let miss = frostbuild_daemon::request(
        &ws.dir,
        &frostbuild_daemon::Request::Run {
            version: frostbuild_daemon::PROTOCOL_VERSION,
            program: ws.dir.join("definitely-missing-frost"),
            args: Vec::new(),
            fast_noop: Some(frostbuild_daemon::FastNoopRequest {
                profile: "debug".into(),
                platform: frostbuild_core::manifest::HOST_PLATFORM.into(),
                key_env: frostbuild_exec::key_environment_snapshot(),
            }),
        },
    )
    .unwrap();
    assert_ne!(miss.code, 0, "a changed input must reject the certificate");
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
#[cfg(unix)]
fn dev_infers_the_artifact_rebuilds_and_restarts_its_process() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::new("watch-restart");
    ws.write(
        "tools/dev-probe",
        "#!/bin/sh\nset -eu\n\"$1\" >> .frost/dev-runs.txt\n",
    );
    std::fs::set_permissions(
        ws.dir.join("tools/dev-probe"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let mut watch = Command::new(frost_bin())
        .arg("-C")
        .arg(&ws.dir)
        .args([
            "dev",
            "app",
            "--debounce-ms",
            "20",
            "--runner",
            "tools/dev-probe",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let runs = ws.dir.join(".frost/dev-runs.txt");
    let wait_for_runs = |minimum: usize| {
        for _ in 0..250 {
            let count = std::fs::read_to_string(&runs)
                .unwrap_or_default()
                .lines()
                .count();
            if count >= minimum {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    };

    let initial = wait_for_runs(1);
    ws.append("src/util.c", "\n/* trigger watch rebuild */\n");
    let restarted = wait_for_runs(2);
    let _ = watch.kill();
    let _ = watch.wait();

    assert!(initial, "dev did not infer and run the initial artifact");
    assert!(restarted, "dev did not restart after a source change");
    let observed = std::fs::read_to_string(runs).unwrap();
    assert!(
        observed.lines().all(|line| line == "frost: 42"),
        "unexpected dev process output: {observed:?}"
    );
}

#[test]
#[cfg(unix)]
fn bazel_query_import_creates_buildable_package_manifests_without_overwrite() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::empty("import-bazel");
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();
    std::fs::create_dir_all(ws.dir.join("lib")).unwrap();
    std::fs::create_dir_all(ws.dir.join("app")).unwrap();
    ws.write("lib/math.cc", "int add(int a, int b) { return a + b; }\n");
    ws.write(
        "app/main.cc",
        "int add(int, int); int main() { return add(20, 22) == 42 ? 0 : 1; }\n",
    );
    ws.write(
        "tools/bazel",
        r#"#!/bin/sh
set -eu
case "$*" in
  *--version*) printf 'bazel 9.1.0\n' ;;
  *--output=build*) printf '# expanded BUILD without configurable attributes\n' ;;
  *--output=xml*)
    /bin/cat <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<query version="2">
  <rule class="cc_library rule" name="//lib:math">
    <list name="srcs"><label value="//lib:math.cc"/></list>
  </rule>
  <rule class="cc_binary rule" name="//app:runner">
    <list name="srcs"><label value="//app:main.cc"/></list>
    <list name="deps"><label value="//lib:math"/></list>
  </rule>
</query>
XML
    ;;
  *) exit 2 ;;
esac
"#,
    );
    std::fs::set_permissions(
        ws.dir.join("tools/bazel"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let (ok, out) = ws.frost(&["import-bazel", "--bazel", "tools/bazel"]);
    assert!(ok, "Bazel import failed:\n{out}");
    assert!(out.contains("2 rules"), "{out}");
    assert!(ws.dir.join("frost.toml").is_file());
    assert!(ws.dir.join("lib/frost.toml").is_file());
    assert!(ws.dir.join("app/frost.toml").is_file());

    let (ok, out) = ws.frost(&["build"]);
    assert!(ok, "imported manifests did not build:\n{out}");

    let (ok, out) = ws.frost(&["import-bazel", "--bazel", "tools/bazel"]);
    assert!(!ok, "a second import overwrote manifests:\n{out}");
    assert!(out.contains("refusing to overwrite"), "{out}");
}

#[test]
#[cfg(unix)]
fn bazel_dev_rebuilds_and_restarts_only_after_success() {
    use std::os::unix::fs::PermissionsExt;

    let ws = Workspace::empty("bazel-dev");
    std::fs::create_dir_all(ws.dir.join("tools")).unwrap();
    ws.write("app.txt", "healthy one\n");
    ws.write(
        "tools/bazel",
        r#"#!/bin/sh
set -eu
mkdir -p .frost
case "$1" in
  build)
    printf '%s\n' "$*" >> .frost/bazel-builds.txt
    if grep -q broken app.txt; then
      exit 7
    fi
    ;;
  run)
    printf '%s\n' "$*" >> .frost/bazel-runs.txt
    trap 'exit 0' INT TERM
    while :; do
      printf tick >> .frost/bazel-heartbeat.txt
      sleep 0.02
    done
    ;;
  *) exit 2 ;;
esac
"#,
    );
    std::fs::set_permissions(
        ws.dir.join("tools/bazel"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let mut dev = Command::new(frost_bin())
        .arg("-C")
        .arg(&ws.dir)
        .args([
            "bazel-dev",
            "//app:server",
            "--bazel",
            "tools/bazel",
            "--debounce-ms",
            "20",
            "--bazel-arg=--config=dev",
            "--",
            "--port",
            "3000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let builds = ws.dir.join(".frost/bazel-builds.txt");
    let runs = ws.dir.join(".frost/bazel-runs.txt");
    let heartbeats = ws.dir.join(".frost/bazel-heartbeat.txt");
    let wait_for_lines = |path: &Path, minimum: usize| {
        for _ in 0..250 {
            if std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .count()
                >= minimum
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    };
    let wait_for_stable_counts = |minimum_runs: usize| {
        let mut previous = (0, 0);
        let mut stable_checks = 0;
        for _ in 0..500 {
            let current = (
                std::fs::read_to_string(&builds)
                    .unwrap_or_default()
                    .lines()
                    .count(),
                std::fs::read_to_string(&runs)
                    .unwrap_or_default()
                    .lines()
                    .count(),
            );
            if current.1 >= minimum_runs && current == previous {
                stable_checks += 1;
                if stable_checks >= 25 {
                    return Some(current);
                }
            } else {
                stable_checks = 0;
            }
            previous = current;
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    };

    assert!(
        wait_for_lines(&runs, 1),
        "initial Bazel target did not start"
    );
    ws.write("app.txt", "healthy two\n");
    assert!(
        wait_for_lines(&runs, 2),
        "successful rebuild did not restart"
    );
    // Native watchers may deliver a second event for the same editor write
    // after the first debounce window, especially on a heavily loaded host.
    // Establish the healthy baseline only after all such successful restarts
    // settle; the assertion below is specifically about the later failed
    // build, not about the backend's event coalescing behavior.
    let settled = wait_for_stable_counts(2);
    assert!(
        settled.is_some(),
        "successful Bazel rebuild/restart stream did not settle: {} builds / {} runs",
        std::fs::read_to_string(&builds)
            .unwrap_or_default()
            .lines()
            .count(),
        std::fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .count()
    );
    let (healthy_builds, healthy_runs) = settled.unwrap();

    ws.write("app.txt", "broken\n");
    assert!(
        wait_for_lines(&builds, healthy_builds + 1),
        "broken change was not rebuilt"
    );
    let before = std::fs::metadata(&heartbeats).unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let after = std::fs::metadata(&heartbeats).unwrap().len();
    assert!(
        after > before,
        "failed build stopped the last healthy process"
    );
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        healthy_runs,
        "failed build launched a replacement process"
    );

    unsafe {
        libc::kill(dev.id() as i32, libc::SIGINT);
    }
    let status = dev.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    let run_log = std::fs::read_to_string(runs).unwrap();
    assert!(run_log.contains("--config=dev //app:server -- --port 3000"));
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
    let (out, first_warm) = run_with(&one);
    assert_eq!(first_warm, "frost: 43\n");
    assert!(out.contains("up to date"), "{out}");
    assert!(
        ws.dir.join(".frost/noop-debug.bin").is_file(),
        "the environment regression must exercise the fast no-op path"
    );

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
    let manifest = std::fs::read_to_string(dir.join("frost.toml")).unwrap();
    assert!(manifest.contains("[profile.debug]"), "{manifest}");
    assert!(
        manifest.contains("cflags = [\"-O0\", \"-g\"]"),
        "{manifest}"
    );

    let (ok, out) = frost(&["build"]);
    assert!(ok, "the scaffold must build as written:\n{out}");
    let run = Command::new(dir.join(".frost/bin/debug").join(dir.file_name().unwrap()))
        .output()
        .expect("run built binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");

    let target = dir.file_name().unwrap().to_str().unwrap();
    let (ok, out) = frost(&["run", target]);
    assert!(ok, "run must build and execute the target:\n{out}");
    assert!(out.contains("frost: run"), "{out}");
    assert!(out.ends_with("42\n"), "{out}");

    std::fs::create_dir_all(dir.join("tools")).unwrap();
    std::fs::write(
        dir.join("tools/fake-gdb"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > debug-argv.txt\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("tools/fake-gdb"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let (ok, out) = frost(&[
            "debug",
            target,
            "--debugger",
            "tools/fake-gdb",
            "--",
            "hello",
        ]);
        assert!(ok, "debug launch failed:\n{out}");
        assert!(out.contains("frost: debug"), "{out}");
        let argv = std::fs::read_to_string(dir.join("debug-argv.txt")).unwrap();
        assert!(argv.lines().next() == Some("--args"), "{argv}");
        assert!(argv.contains(".frost/bin/debug"), "{argv}");
        assert!(argv.lines().last() == Some("hello"), "{argv}");
    }

    let (ok, out) = frost(&["ide", target, "--dry-run"]);
    assert!(ok, "IDE preview failed:\n{out}");
    assert!(out.contains("\"type\": \"cppdbg\""), "{out}");
    assert!(out.contains(&format!("frost: build {target}")), "{out}");
    let (ok, out) = frost(&["ide", target]);
    assert!(ok, "IDE generation failed:\n{out}");
    let launch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(".vscode/launch.json")).unwrap()).unwrap();
    let tasks: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(".vscode/tasks.json")).unwrap()).unwrap();
    assert_eq!(launch["configurations"][0]["type"], "cppdbg");
    assert_eq!(
        launch["configurations"][0]["preLaunchTask"],
        format!("frost: build {target}")
    );
    assert_eq!(tasks["tasks"][0]["command"], "frost");
    let (ok, out) = frost(&["ide", target]);
    assert!(!ok, "IDE generation must not overwrite user files");
    assert!(out.contains("--dry-run"), "{out}");

    let (ok, out) = frost(&["doctor", "--json"]);
    assert!(ok, "doctor rejected a buildable scaffold:\n{out}");
    let diagnosis: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(diagnosis["status"], "ready");
    assert!(diagnosis["required_tools"].as_array().unwrap().len() >= 4);

    // init refuses to clobber an existing manifest, and says how to look
    // without writing.
    let (ok, out) = frost(&["init"]);
    assert!(!ok, "{out}");
    assert!(out.contains("--dry-run"), "{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_java_writes_a_runnable_deterministic_jar_manifest() {
    if Command::new("javac").arg("-version").output().is_err()
        || Command::new("java").arg("-version").output().is_err()
    {
        eprintln!("skipping Java init E2E: javac and java are required");
        return;
    }

    let ws = Workspace::empty("init-java");
    std::fs::create_dir_all(ws.dir.join("src/main/java/com/example")).unwrap();
    ws.write(
        "src/main/java/com/example/App.java",
        "package com.example;\n\
         public final class App {\n\
           public static void main(String[] args) {\n\
             System.out.println(\"java-init-ok\");\n\
           }\n\
         }\n",
    );

    // The generated manifest intentionally names `frost`, just as a user's
    // installed manifest will. Cargo exposes the test binary by absolute path,
    // so put its directory on PATH to exercise that installed-command shape.
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let frost_parent = Path::new(frost_bin()).parent().unwrap().to_path_buf();
    let path = std::env::join_paths(
        std::iter::once(frost_parent).chain(std::env::split_paths(&current_path)),
    )
    .unwrap();
    let frost = |args: &[&str]| {
        let output = Command::new(frost_bin())
            .arg("-C")
            .arg(&ws.dir)
            .args(args)
            .env("PATH", &path)
            .output()
            .expect("spawn frost");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string()
                + &String::from_utf8_lossy(&output.stderr),
        )
    };

    let (ok, out) = frost(&["init"]);
    assert!(ok, "Java auto-detection failed:\n{out}");
    assert!(out.contains("1 Java source file(s)"), "{out}");
    assert!(out.contains("entry point: com.example.App"), "{out}");

    let manifest_text = std::fs::read_to_string(ws.dir.join("frost.toml")).unwrap();
    let manifest = frostbuild_core::manifest::Manifest::parse_str(&manifest_text).unwrap();
    let target = ws.dir.file_name().unwrap().to_str().unwrap();
    let spec = &manifest.targets[target];
    assert_eq!(spec.tool.as_deref(), Some("javac"));
    assert_eq!(spec.steps.len(), 1);
    assert_eq!(spec.steps[0].tool, "frost");
    assert!(
        spec.steps[0]
            .args
            .windows(2)
            .any(|args| args == ["--main-class", "com.example.App"]),
        "{:?}",
        spec.steps[0].args
    );

    let (ok, out) = frost(&["build"]);
    assert!(ok, "generated Java manifest did not build:\n{out}");
    let jar = ws
        .dir
        .join(".frost/out/debug")
        .join(format!("{target}.jar"));
    let direct = Command::new("java")
        .arg("-jar")
        .arg(&jar)
        .output()
        .expect("run generated Java JAR");
    assert!(direct.status.success(), "{direct:?}");
    assert_eq!(String::from_utf8_lossy(&direct.stdout), "java-init-ok\n");

    let (ok, out) = frost(&["run", target]);
    assert!(ok, "frost run rejected generated Java target:\n{out}");
    assert!(out.contains("`-- runtime   Java"), "{out}");
    assert!(out.ends_with("java-init-ok\n"), "{out}");

    let (ok, out) = frost(&[
        "debug",
        target,
        "--debugger",
        frost_bin(),
        "--print",
        "--",
        "argument",
    ]);
    assert!(ok, "Java debug preview rejected generated JAR:\n{out}");
    assert!(out.contains("Java/jdb"), "{out}");
    assert!(out.contains("-classpath"), "{out}");
    assert!(out.contains("com.example.App"), "{out}");

    let _ = std::fs::remove_dir_all(&ws.dir);
}

#[test]
fn init_mixed_workspace_requires_an_explicit_language() {
    let ws = Workspace::empty("init-mixed");
    std::fs::create_dir_all(ws.dir.join("src")).unwrap();
    ws.write("src/main.c", "int main(void) { return 0; }\n");
    ws.write(
        "src/App.java",
        "public final class App { public static void main(String[] args) {} }\n",
    );

    let (ok, out) = ws.frost(&["init"]);
    assert!(!ok, "mixed source families must not be guessed:\n{out}");
    assert!(out.contains("--language native"), "{out}");
    assert!(out.contains("--language java"), "{out}");
    assert!(!ws.dir.join("frost.toml").exists());

    let (ok, out) = ws.frost(&["init", "--language", "java", "--dry-run"]);
    assert!(ok, "explicit Java preview failed:\n{out}");
    assert!(out.contains("inputs = [\"src/App.java\"]"), "{out}");
    assert!(!out.contains("src/main.c"), "{out}");
    assert!(!ws.dir.join("frost.toml").exists());
}

#[test]
fn doctor_separates_missing_required_tools_from_optional_integrations() {
    let ws = Workspace::empty("doctor-missing");
    ws.write("input.txt", "input\n");
    ws.write(
        "frost.toml",
        r#"[workspace]
default_targets = ["artifact"]

[toolchain.tools]
missing = "definitely-not-a-real-frost-tool"

[target.artifact]
kind = "command"
tool = "missing"
args = ["${in}", "${out}"]
inputs = ["input.txt"]
outputs = [".frost/out/${config}/artifact.txt"]
sandbox = false
"#,
    );
    let (ok, out) = ws.frost(&["doctor", "--json"]);
    assert!(!ok, "missing required tool must make doctor nonzero");
    let diagnosis: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(diagnosis["status"], "blocked");
    let tools = diagnosis["required_tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| {
        tool["configured"] == "definitely-not-a-real-frost-tool"
            && tool["available"] == false
            && tool["required"] == true
    }));
    assert!(diagnosis["optional_integrations"].is_array());
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
        "init only auto-detects artifact-safe source families"
    );
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("no safely scaffoldable C/C++ or Java sources"),
        "{text}"
    );
    assert!(text.contains("kind = \"command\""), "{text}");
    assert!(text.contains("TypeScript"), "{text}");

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
