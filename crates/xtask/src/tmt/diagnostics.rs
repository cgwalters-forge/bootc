//! Opt-in failure diagnostics, adapted from cgwalters-bot/bootc 84a98fade01f.
//! Reuse existing captures; never publish raw inspect output or command stderr.
use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use camino::Utf8Path;
use serde_json::{Value, json};
use xshell::{Shell, cmd};

// Extra guest journal collection may disclose guest data and delay cleanup: opt in
// explicitly. Bounds prevent a wedged guest/tool from exhausting time or artifacts.
const ENABLE: &str = "BOOTC_TMT_FAILURE_DIAGNOSTICS";
const MAX_BYTES: u64 = 256 * 1024;
const MAX_RESULTS: usize = 64;
const MAX_LOGS: usize = 16;
const MAX_FILES: usize = 128;
const COMMAND_TIMEOUT: &str = "10s";

pub(super) struct Plan<'a> {
    pub name: &'a str,
    pub vm: &'a str,
    pub results: &'a Utf8Path,
    pub dir: &'a Utf8Path,
    pub capture_supported: bool,
    pub port: u16,
    pub key: &'a Utf8Path,
}

// This is the production post-plan path, also exercised with real fake processes.
pub(super) fn finish_plan(sh: &Shell, failed: bool, preserve: bool, plan: &Plan<'_>) {
    if failed && sh.var(ENABLE).ok().as_deref() == Some("1") {
        if let Err(e) = collect(sh, plan) {
            eprintln!("Warning: failure diagnostics unavailable: {e}");
        }
    }
    if !preserve {
        let vm = plan.vm;
        if let Err(e) = cmd!(sh, "bcvk libvirt rm --stop --force {vm}")
            .ignore_stderr()
            .ignore_status()
            .run()
        {
            eprintln!("Warning: Failed to cleanup VM {vm}: {e}");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    Inspect,
    Reachable,
    PreviousJournal,
}

fn run(sh: &Shell, request: Request, plan: &Plan<'_>) -> Result<String> {
    let port = plan.port.to_string();
    let args = match request {
        Request::Inspect => vec!["bcvk", "libvirt", "inspect", plan.vm, "--format=json"],
        Request::Reachable | Request::PreviousJournal => vec![
            "ssh",
            "-i",
            plan.key.as_str(),
            "-p",
            &port,
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "ConnectionAttempts=1",
            "root@localhost",
            if request == Request::Reachable {
                "true"
            } else {
                "journalctl -b -1 --no-pager --lines=1000 --output=short-iso"
            },
        ],
    };
    bounded_command(sh, &args, COMMAND_TIMEOUT)
}

/// Errors deliberately contain neither command output nor arguments (key path).
fn bounded_command(sh: &Shell, args: &[&str], timeout: &str) -> Result<String> {
    let mut command: Command = sh
        .cmd("timeout")
        .args(["--kill-after=1s", timeout])
        .args(args)
        .into();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("collector could not start")?;
    let mut output = Vec::new();
    // Closing the pipe at the limit also bounds a writer ignoring SIGPIPE via timeout.
    let read = child
        .stdout
        .take()
        .context("collector stdout missing")?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut output);
    let status = child.wait().context("waiting for collector")?;
    read.context("reading collector output")?;
    anyhow::ensure!(
        output.len() as u64 <= MAX_BYTES,
        "collector output exceeded byte limit"
    );
    anyhow::ensure!(
        status.success(),
        "collector exited {status} (124/137 indicates timeout)"
    );
    Ok(String::from_utf8(output).context("collector output was not UTF-8")?)
}

fn read_bounded(path: &Utf8Path) -> Result<String> {
    let metadata = std::fs::metadata(path).context("missing or unreadable file")?;
    anyhow::ensure!(metadata.is_file(), "not a regular file");
    anyhow::ensure!(metadata.len() <= MAX_BYTES, "file exceeded byte limit");
    let mut data = String::new();
    std::fs::File::open(path)
        .context("unreadable file")?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut data)
        .context("unreadable file")?;
    anyhow::ensure!(data.len() as u64 <= MAX_BYTES, "file exceeded byte limit");
    Ok(data)
}

fn vm_state(raw: &str) -> Result<Value> {
    let data: Value = serde_json::from_str(raw).context("invalid inspect JSON")?;
    // Exclude image, labels, paths and key material, including unexpected fields.
    let mut state = json!({});
    for field in ["memory_mb", "vcpus", "ssh_port"] {
        if let Some(value) = data[field].as_u64() {
            state[field] = json!(value);
        }
    }
    state["state"] = json!(match data["state"].as_str() {
        Some(
            s @ ("running" | "paused" | "shutoff" | "shut off" | "shut-off" | "stopped" | "crashed"
            | "blocked" | "shutdown" | "in shutdown" | "pmsuspended"),
        ) => s,
        _ => "unknown",
    });
    Ok(state)
}

fn log_evidence(text: &str, count: &mut Option<u64>, step: &mut &'static str) {
    for line in text.lines() {
        // Match TMT's exception, not guest/application output mentioning timeouts.
        if line.trim_start().starts_with("RebootTimeoutError:") {
            *step = "reboot wait timeout (TMT exception)";
        }
        for marker in ["TMT_REBOOT_COUNT=", "with reboot count "] {
            if let Some(value) = line
                .split_once(marker)
                .and_then(|(_, v)| v.split_whitespace().next())
            {
                *count = value.parse().ok().or(*count);
            }
        }
    }
}

fn failed_tests(path: &Utf8Path) -> Result<Value> {
    let results: Vec<serde_yaml::Value> = serde_yaml::from_str(&read_bounded(path)?)?;
    let mut tests = Vec::new();
    let mut remaining = MAX_LOGS;
    for result in results.iter().take(MAX_RESULTS) {
        let status = result["result"].as_str().unwrap_or("unknown");
        if matches!(status, "pass" | "info" | "skip") {
            continue;
        }
        // TMT's executor records this exact note on RebootTimeoutError.
        let mut step = if result["note"]
            .as_sequence()
            .is_some_and(|notes| notes.iter().any(|n| n.as_str() == Some("reboot timeout")))
        {
            "reboot timeout (TMT result)"
        } else {
            "unknown"
        };
        let mut count = result["reboot-count"].as_u64();
        let mut logs = Vec::new();
        let references = result["log"].as_sequence();
        if let Some(references) = references {
            for log in references.iter().take(remaining) {
                let read = (|| -> Result<String> {
                    let relative =
                        Utf8Path::new(log.as_str().context("rejected non-string log reference")?);
                    anyhow::ensure!(
                        !relative.as_str().is_empty()
                            && relative
                                .components()
                                .all(|c| matches!(c, camino::Utf8Component::Normal(_))),
                        "rejected log path"
                    );
                    let parent = path.parent().context("missing results parent")?;
                    let candidate = parent
                        .join(relative)
                        .canonicalize()
                        .context("missing or unreadable referenced log")?;
                    anyhow::ensure!(
                        candidate.starts_with(parent.canonicalize()?),
                        "rejected escaping log symlink"
                    );
                    read_bounded(Utf8Path::from_path(&candidate).context("non-UTF8 log path")?)
                })();
                let state = match read {
                    Ok(text) => {
                        log_evidence(&text, &mut count, &mut step);
                        "read".to_owned()
                    }
                    Err(e) => format!("{e:#}"),
                };
                // Do not copy log contents into the manifest.
                logs.push(json!({"reference": log.as_str().unwrap_or("unknown"), "status": state}));
            }
        }
        remaining -= logs.len();
        tests.push(json!({
            "name": result["name"].as_str().unwrap_or("unknown"), "result": status,
            "reboot_count": count.map_or(json!("unknown"), |v| json!(v)),
            "reboot_command_or_step": step, "logs": logs,
            "logs_truncated": references.is_some_and(|r| r.len() > logs.len()),
        }));
    }
    if tests.is_empty() {
        tests.push(unknown_test());
    }
    Ok(json!({"tests": tests, "results_truncated": results.len() > MAX_RESULTS}))
}

fn unknown_test() -> Value {
    json!({"name": "unknown", "reboot_count": "unknown", "reboot_command_or_step": "unknown"})
}

fn capture_files(dir: &Utf8Path, supported: bool) -> Value {
    let mut files = Vec::new();
    let mut console = false;
    let mut journal = false;
    let mut truncated = false;
    let scan = (|| -> Result<()> {
        for (index, entry) in std::fs::read_dir(dir)?.enumerate() {
            if index >= MAX_FILES {
                truncated = true;
                break;
            }
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file()
                && (name.starts_with("console") || name.starts_with("journal"))
            {
                let size = entry.metadata()?.len();
                console |= name.starts_with("console") && size > 0;
                journal |= name.starts_with("journal") && size > 0;
                files.push(json!({"file": name, "bytes": size}));
            }
        }
        Ok(())
    })();
    files.sort_by_key(|v| v["file"].as_str().unwrap_or_default().to_owned());
    json!({"files": files, "inventory_truncated": truncated,
        "support": if supported { "bcvk --log-dir supported" } else { "missing bcvk --log-dir support (old or unavailable bcvk)" },
        "console": if console { "retained in place" } else { "missing or empty console capture" },
        "journal": if journal { "retained in place, including rotated files" } else { "missing or empty journal capture" },
        "scan_error": scan.err().map(|e| e.to_string()),
    })
}

fn collect(sh: &Shell, plan: &Plan<'_>) -> Result<()> {
    std::fs::create_dir_all(plan.dir)?;
    let mut bundle = json!({"plan": plan.name, "vm": plan.vm,
        "captures": capture_files(plan.dir, plan.capture_supported)});
    bundle["failure"] = failed_tests(plan.results)
        .unwrap_or_else(|e| json!({"tests": [unknown_test()], "unavailable": format!("{e:#}")}));
    bundle["vm_state"] = run(sh, Request::Inspect, plan)
        .and_then(|s| vm_state(&s))
        .unwrap_or_else(|e| json!({"unavailable": e.to_string()}));
    bundle["previous_boot_journal"] = match run(sh, Request::Reachable, plan) {
        Err(e) => json!({"unavailable": format!("SSH unreachable: {e}")}),
        Ok(_) => match run(sh, Request::PreviousJournal, plan) {
            Ok(text) if !text.trim().is_empty() && text.trim() != "-- No entries --" => {
                match std::fs::write(plan.dir.join("previous-boot-journal.txt"), text) {
                    Ok(()) => {
                        json!({"file": "previous-boot-journal.txt", "limit": "1000 lines, 256 KiB"})
                    }
                    Err(e) => {
                        json!({"unavailable": format!("saving previous journal failed: {e}")})
                    }
                }
            }
            Ok(_) => json!({"unavailable": "previous boot has no journal entries"}),
            Err(e) => json!({"unavailable": format!("journalctl -b -1 failed: {e}")}),
        },
    };
    std::fs::write(
        plan.dir.join("failure-diagnostics.json"),
        serde_json::to_vec_pretty(&bundle)?,
    )?;
    eprintln!(
        "Failure diagnostics: {}/failure-diagnostics.json (existing captures retained)",
        plan.dir
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn executable(root: &Utf8Path, name: &str, body: &str) -> Result<()> {
        let path = root.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    #[test]
    fn production_lifecycle() -> Result<()> {
        for scenario in [
            "failure",
            "success",
            "preserve",
            "disabled",
            "unreachable",
            "old-bcvk",
            "missing-captures",
            "missing-tool",
        ] {
            let temp = tempfile::tempdir()?;
            let root = Utf8Path::from_path(temp.path()).unwrap();
            let sh = Shell::new()?;
            let _cwd = sh.push_dir(root);
            let _path = sh.push_env("PATH", format!("{root}:/usr/bin:/bin"));
            let _enable = sh.push_env(ENABLE, if scenario == "disabled" { "0" } else { "1" });
            let _scenario = sh.push_env("SCENARIO", scenario);
            executable(
                root,
                "bcvk",
                "if [ \"$2\" = inspect ]; then cat inspect.json; else test ! -e expected || test -s captures/failure-diagnostics.json; printf 'cleanup\\n' >> events; fi",
            )?;
            executable(
                root,
                "ssh",
                "printf 'ssh\\n' >> events\n[ \"$SCENARIO\" != unreachable ] || exit 255\nfor arg do last=$arg; done\n[ \"$last\" = true ] || printf 'previous boot evidence\\n'",
            )?;
            if scenario == "missing-tool" {
                executable(root, "bcvk", "exit 127")?;
            }
            let key = [
                "-----BEGIN",
                " OPENSSH PRIVATE",
                " KEY-----\nsecret\n",
                "-----END",
                " OPENSSH PRIVATE",
                " KEY-----",
            ]
            .concat();
            std::fs::write(root.join("key"), &key)?;
            std::fs::write(root.join("inspect.json"), json!({"state": "shut off", "vcpus": 2,
                "ssh_private_key": key, "labels": {"secret": "hidden label"}, "image": "hidden image"}).to_string())?;
            let dir = root.join("captures");
            if !matches!(scenario, "old-bcvk" | "missing-captures") {
                std::fs::create_dir(&dir)?;
                for name in ["console.txt", "journal.json", "journal.json.1"] {
                    std::fs::write(dir.join(name), "existing evidence")?;
                }
            }
            let results = root.join("results.yaml");
            std::fs::write(
                &results,
                "- name: /test-reinstall\n  result: error\n  note: [reboot timeout]\n  log: [output.txt]\n",
            )?;
            std::fs::write(
                root.join("output.txt"),
                "Running test 'reinstall' with reboot count 2 and test restart count 0.\n",
            )?;
            let failed = scenario != "success";
            let collecting = failed && scenario != "disabled";
            if collecting {
                std::fs::write(root.join("expected"), "")?;
            }
            let plan = Plan {
                name: "/plan-reinstall",
                vm: "vm-test",
                results: &results,
                dir: &dir,
                capture_supported: scenario != "old-bcvk",
                port: 2222,
                key: &root.join("key"),
            };
            finish_plan(&sh, failed, scenario == "preserve", &plan);
            let events = std::fs::read_to_string(root.join("events")).unwrap_or_default();
            assert_eq!(
                events.contains("cleanup"),
                !matches!(scenario, "preserve" | "missing-tool"),
                "{scenario}"
            );
            assert_eq!(
                dir.join("failure-diagnostics.json").exists(),
                collecting,
                "{scenario}"
            );
            if !collecting {
                assert_eq!(events, "cleanup\n");
                continue;
            }
            let raw = read_bounded(&dir.join("failure-diagnostics.json"))?;
            for secret in [&key, "ssh_private_key", "hidden label", "hidden image"] {
                assert!(!raw.contains(secret));
            }
            let bundle: Value = serde_json::from_str(&raw)?;
            assert_eq!(bundle["plan"], "/plan-reinstall");
            assert_eq!(bundle["failure"]["tests"][0]["name"], "/test-reinstall");
            assert_eq!(bundle["failure"]["tests"][0]["reboot_count"], 2);
            assert_eq!(
                bundle["failure"]["tests"][0]["reboot_command_or_step"],
                "reboot timeout (TMT result)"
            );
            if scenario != "missing-tool" {
                assert_eq!(bundle["vm_state"], json!({"state": "shut off", "vcpus": 2}));
            }
            if scenario == "unreachable" {
                assert_eq!(events, "ssh\ncleanup\n");
                assert!(
                    bundle["previous_boot_journal"]["unavailable"]
                        .as_str()
                        .unwrap()
                        .starts_with("SSH unreachable")
                );
                assert!(!dir.join("previous-boot-journal.txt").exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(dir.join("previous-boot-journal.txt"))?,
                    "previous boot evidence\n"
                );
            }
            if matches!(scenario, "old-bcvk" | "missing-captures") {
                assert_eq!(
                    bundle["captures"]["console"],
                    "missing or empty console capture"
                );
            } else {
                assert_eq!(bundle["captures"]["files"].as_array().unwrap().len(), 3);
                assert_eq!(
                    std::fs::read_to_string(dir.join("journal.json.1"))?,
                    "existing evidence"
                );
            }
            if scenario == "old-bcvk" {
                assert!(raw.contains("missing bcvk --log-dir support"));
            }
        }
        Ok(())
    }

    #[test]
    fn production_command_bounds() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let sh = Shell::new()?;
        let _path = sh.push_env("PATH", format!("{root}:/usr/bin:/bin"));
        for (name, body, expected) in [
            ("hang", "sleep 30", "timeout"),
            ("flood", "yes x", "byte limit"),
            ("fail", "printf 'secret stderr' >&2; exit 7", "exited"),
        ] {
            executable(root, name, body)?;
            let start = std::time::Instant::now();
            let error = bounded_command(&sh, &[name], "0.1s")
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("secret stderr"));
            assert!(start.elapsed() < std::time::Duration::from_secs(3));
        }
        Ok(())
    }

    #[test]
    fn referenced_log_failures_and_specific_evidence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let path = root.join("results.yaml");
        std::fs::write(root.join("oversize"), "x".repeat(MAX_BYTES as usize + 1))?;
        std::fs::write(root.join("unreadable"), [0xff])?;
        std::fs::create_dir(root.join("directory"))?;
        symlink("/dev/null", root.join("escape"))?;
        for (log, expected) in [
            ("missing", "missing"),
            ("oversize", "byte limit"),
            ("unreadable", "unreadable"),
            ("directory", "regular file"),
            ("../outside", "rejected"),
            ("escape", "rejected"),
        ] {
            std::fs::write(
                &path,
                format!("- name: test\n  result: error\n  log: [{log}]\n"),
            )?;
            let data = failed_tests(&path)?;
            assert!(
                data["tests"][0]["logs"][0]["status"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{data}"
            );
            assert_eq!(data["tests"][0]["reboot_count"], "unknown");
            assert_eq!(data["tests"][0]["reboot_command_or_step"], "unknown");
        }
        for (note, text, expected) in [
            (
                "application reboot timeout",
                "reboot timed out\nnetwork timeout during reboot",
                "unknown",
            ),
            (
                "other",
                "RebootTimeoutError: guest did not return",
                "reboot wait timeout (TMT exception)",
            ),
            (
                "reboot timeout",
                "unrelated timeout",
                "reboot timeout (TMT result)",
            ),
        ] {
            std::fs::write(root.join("output"), text)?;
            std::fs::write(
                &path,
                format!("- name: test\n  result: error\n  note: [{note}]\n  log: [output]\n"),
            )?;
            assert_eq!(
                failed_tests(&path)?["tests"][0]["reboot_command_or_step"],
                expected
            );
        }
        for state in ["shut off", "shutoff", "shut-off", "stopped", "running"] {
            assert_eq!(
                vm_state(&json!({"state": state}).to_string())?["state"],
                state
            );
        }
        std::fs::remove_file(&path)?;
        assert!(failed_tests(&path).is_err());
        std::fs::write(&path, "x".repeat(MAX_BYTES as usize + 1))?;
        assert!(
            failed_tests(&path)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        Ok(())
    }
}
