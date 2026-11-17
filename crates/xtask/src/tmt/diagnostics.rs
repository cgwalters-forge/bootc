//! Failure-only diagnostics. Reuse bcvk's captures; never serialize inspect output.
use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use camino::Utf8Path;
use serde_json::{Value, json};

// Prevent a broken collector from holding up cleanup or filling the artifact upload.
const MAX_BYTES: u64 = 256 * 1024;
const MAX_RESULTS: usize = 64;
const MAX_FILES: usize = 128;
const COMMAND_TIMEOUT: &str = "10s";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    Inspect,
    Reachable,
    PreviousJournal,
}

pub(super) fn after_plan(failed: bool, collect: impl FnOnce(), cleanup: impl FnOnce()) {
    if failed {
        collect();
    }
    cleanup();
}

/// No command stderr or inspect data is included in errors: either can contain credentials.
pub(super) fn run(request: Request, vm: &str, port: u16, key: &Utf8Path) -> Result<String> {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=1s", COMMAND_TIMEOUT]);
    match request {
        Request::Inspect => {
            command.args(["bcvk", "libvirt", "inspect", vm, "--format=json"]);
        }
        Request::Reachable | Request::PreviousJournal => {
            command.args(["ssh", "-i"]).arg(key).args([
                "-p",
                &port.to_string(),
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
            ]);
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("collector could not start (tool may be missing)")?;
    let mut output = Vec::new();
    // Drop the pipe at the limit; timeout still bounds the child if it ignores SIGPIPE.
    let read = child
        .stdout
        .take()
        .context("collector stdout missing")?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut output);
    let status = child.wait().context("waiting for collector")?;
    read.context("reading collector output")?;
    anyhow::ensure!(
        status.success(),
        "collector exited {status} (124/137 indicates timeout)"
    );
    anyhow::ensure!(
        output.len() as u64 <= MAX_BYTES,
        "collector output exceeded byte limit"
    );
    Ok(String::from_utf8(output).context("collector output was not UTF-8")?)
}

fn read_bounded(path: &Utf8Path) -> Result<String> {
    anyhow::ensure!(std::fs::metadata(path)?.is_file(), "not a regular file");
    let mut data = String::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut data)?;
    anyhow::ensure!(data.len() as u64 <= MAX_BYTES, "file exceeded byte limit");
    Ok(data)
}

fn vm_state(raw: &str) -> Result<Value> {
    let data: Value = serde_json::from_str(raw).context("invalid inspect JSON")?;
    // Deliberately exclude image, labels, disk paths and all key material.
    let mut state = json!({});
    for field in ["memory_mb", "vcpus", "ssh_port"] {
        if let Some(value) = data[field].as_u64() {
            state[field] = json!(value);
        }
    }
    state["state"] = json!(match data["state"].as_str() {
        Some(s @ ("running" | "paused" | "shutoff" | "crashed" | "blocked" | "shutdown")) => s,
        _ => "unknown",
    });
    Ok(state)
}

fn failed_tests(path: &Utf8Path) -> Result<Value> {
    let data = read_bounded(path)?;
    let results: Vec<serde_yaml::Value> = serde_yaml::from_str(&data)?;
    let mut tests = Vec::new();
    for result in results.iter().take(MAX_RESULTS) {
        let status = result["result"].as_str().unwrap_or("unknown");
        if matches!(status, "pass" | "info" | "skip") {
            continue;
        }
        let mut step = if result["note"].as_sequence().is_some_and(|notes| {
            notes.iter().any(|n| {
                n.as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("reboot timeout"))
            })
        }) {
            "reboot timeout (from TMT result)"
        } else {
            "unknown"
        };
        let mut count = result["reboot-count"].as_u64();
        // TMT log paths are relative to execute/. Reject traversal and escaping symlinks.
        if let Some(logs) = result["log"].as_sequence() {
            for log in logs.iter().take(MAX_RESULTS).filter_map(|v| v.as_str()) {
                let relative = Utf8Path::new(log);
                if relative
                    .components()
                    .any(|c| !matches!(c, camino::Utf8Component::Normal(_)))
                {
                    continue;
                }
                let Some(parent) = path.parent() else {
                    continue;
                };
                let log_path = parent.join(relative);
                let safe = log_path
                    .canonicalize()
                    .ok()
                    .zip(parent.canonicalize().ok())
                    .is_some_and(|(log, root)| log.starts_with(root));
                if !safe {
                    continue;
                }
                if let Ok(text) = read_bounded(&log_path) {
                    for line in text.lines() {
                        let lower = line.to_ascii_lowercase();
                        if lower.contains("reboot")
                            && (lower.contains("timeout") || lower.contains("timed out"))
                        {
                            step = "reboot wait timeout (from TMT log)";
                        }
                        if let Some(value) = line
                            .split_once("TMT_REBOOT_COUNT=")
                            .and_then(|(_, v)| v.split_whitespace().next())
                        {
                            count = value.parse().ok().or(count);
                        }
                        // TMT's wrapper emits this marker before each invocation.
                        if let Some(value) = line
                            .split_once("with reboot count ")
                            .and_then(|(_, v)| v.split_whitespace().next())
                        {
                            count = value.parse().ok().or(count);
                        }
                    }
                }
            }
        }
        tests.push(json!({
            "name": result["name"].as_str().unwrap_or("unknown"),
            "result": status,
            "reboot_count": count.map_or(json!("unknown"), |v| json!(v)),
            "reboot_command_or_step": step,
        }));
    }
    if tests.is_empty() {
        tests.push(json!({"name": "unknown", "reboot_count": "unknown", "reboot_command_or_step": "unknown"}));
    }
    Ok(json!({"tests": tests, "results_truncated": results.len() > MAX_RESULTS}))
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
            if !entry.file_type()?.is_file() {
                continue;
            }
            if name.starts_with("console") || name.starts_with("journal") {
                let size = entry.metadata()?.len();
                console |= name.starts_with("console") && size > 0;
                journal |= name.starts_with("journal") && size > 0;
                files.push(json!({"file": name, "bytes": size}));
            }
        }
        Ok(())
    })();
    files.sort_by_key(|v| v["file"].as_str().unwrap_or_default().to_owned());
    json!({
        "directory": dir, "files": files, "inventory_truncated": truncated,
        "support": if supported { "bcvk --log-dir supported" } else { "missing bcvk --log-dir support (old or unavailable bcvk)" },
        "console": if console { "retained in place" } else { "missing or empty console capture" },
        "journal": if journal { "retained in place, including rotated files" } else { "missing or empty journal capture" },
        "scan_error": scan.err().map(|e| e.to_string()),
    })
}

pub(super) fn collect(
    plan: &str,
    vm: &str,
    results: &Utf8Path,
    dir: &Utf8Path,
    supported: bool,
    mut command: impl FnMut(Request) -> Result<String>,
) {
    let save = (|| -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut bundle = json!({
            "plan": plan, "vm": vm, "tmt_results": results,
            "captures": capture_files(dir, supported),
        });
        bundle["failure"] = failed_tests(results).unwrap_or_else(|e| json!({
            "tests": [{"name": "unknown", "reboot_count": "unknown", "reboot_command_or_step": "unknown"}],
            "unavailable": e.to_string(),
        }));
        bundle["vm_state"] = command(Request::Inspect)
            .and_then(|s| vm_state(&s))
            .unwrap_or_else(|e| json!({"unavailable": e.to_string()}));
        bundle["previous_boot_journal"] = match command(Request::Reachable) {
            Err(e) => json!({"unavailable": format!("SSH unreachable: {e}")}),
            Ok(_) => match command(Request::PreviousJournal) {
                Ok(text) if !text.trim().is_empty() && text.trim() != "-- No entries --" => {
                    match std::fs::write(dir.join("previous-boot-journal.txt"), text) {
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
            dir.join("failure-diagnostics.json"),
            serde_json::to_vec_pretty(&bundle)?,
        )?;
        eprintln!(
            "FAILURE DIAGNOSTICS: {dir}/failure-diagnostics.json; CONSOLE AND ROTATED JOURNALS retained in {dir}"
        );
        eprintln!("Capture status: {}", bundle["captures"]);
        Ok(())
    })();
    if let Err(e) = save {
        eprintln!(
            "Warning: failure diagnostics unavailable for {vm}: {e}; existing console/journals remain in {dir}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn failure_collection() -> Result<()> {
        for scenario in [
            "success",
            "reboot-timeout",
            "unreachable",
            "old-bcvk",
            "missing-files",
            "collector-failure",
            "journal-failure",
        ] {
            let temp = tempfile::tempdir()?;
            let root = Utf8Path::from_path(temp.path()).unwrap();
            let dir = root.join("captures");
            std::fs::create_dir(&dir)?;
            let results = root.join("results.yaml");
            std::fs::write(
                &results,
                "- name: /test-reinstall\n  result: error\n  log: [output.txt]\n  note: [reboot timeout]\n",
            )?;
            std::fs::write(
                root.join("output.txt"),
                "Running test 'reinstall' with reboot count 2 and test restart count 0.\nreboot timed out\n",
            )?;
            if !matches!(scenario, "old-bcvk" | "missing-files") {
                for file in [
                    "console.txt",
                    "journal.json",
                    "journal.json.1",
                    "journal-initrd.json",
                ] {
                    std::fs::write(dir.join(file), "retained capture")?;
                }
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
            let events = RefCell::new(Vec::new());
            after_plan(
                scenario != "success",
                || {
                    collect(
                        "/plan-reinstall",
                        "vm-test",
                        &results,
                        &dir,
                        scenario != "old-bcvk",
                        |request| {
                            events.borrow_mut().push(format!("{request:?}"));
                            if scenario == "collector-failure"
                                || (scenario == "unreachable" && request == Request::Reachable)
                                || (scenario == "journal-failure"
                                    && request == Request::PreviousJournal)
                            {
                                anyhow::bail!("fake command failed");
                            }
                            Ok(match request {
                        Request::Inspect => json!({"state": "running", "memory_mb": 4096, "vcpus": 2, "ssh_port": 2222,
                            "ssh_private_key": key, "labels": {"secret": "do not publish"}, "image": "also excluded"}).to_string(),
                        Request::Reachable => String::new(),
                        Request::PreviousJournal => "previous boot evidence\n".to_owned(),
                    })
                        },
                    );
                },
                || {
                    if scenario != "success" {
                        assert!(dir.join("failure-diagnostics.json").exists());
                    }
                    events.borrow_mut().push("cleanup".into());
                },
            );
            let events = events.into_inner();
            assert_eq!(events.last().unwrap(), "cleanup");
            if scenario == "success" {
                assert_eq!(events, ["cleanup"]);
                assert!(!dir.join("failure-diagnostics.json").exists());
                continue;
            }
            let raw = std::fs::read_to_string(dir.join("failure-diagnostics.json"))?;
            assert!(!raw.contains(&key));
            for secret in ["ssh_private_key", "do not publish", "also excluded"] {
                assert!(!raw.contains(secret));
            }
            let bundle: Value = serde_json::from_str(&raw)?;
            assert_eq!(bundle["plan"], "/plan-reinstall");
            assert_eq!(bundle["vm"], "vm-test");
            assert_eq!(bundle["failure"]["tests"][0]["name"], "/test-reinstall");
            assert_eq!(bundle["failure"]["tests"][0]["reboot_count"], 2);
            assert_eq!(
                bundle["failure"]["tests"][0]["reboot_command_or_step"],
                "reboot wait timeout (from TMT log)"
            );
            if matches!(scenario, "unreachable" | "collector-failure") {
                assert!(!events.contains(&"PreviousJournal".to_owned()));
                assert!(
                    bundle["previous_boot_journal"]["unavailable"]
                        .as_str()
                        .unwrap()
                        .contains("SSH unreachable")
                );
            } else if scenario == "journal-failure" {
                assert!(
                    bundle["previous_boot_journal"]["unavailable"]
                        .as_str()
                        .unwrap()
                        .contains("journalctl -b -1 failed")
                );
                assert!(!dir.join("previous-boot-journal.txt").exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(dir.join("previous-boot-journal.txt"))?,
                    "previous boot evidence\n"
                );
            }
            if scenario == "old-bcvk" {
                assert!(raw.contains("missing bcvk --log-dir support"));
            }
            if matches!(scenario, "old-bcvk" | "missing-files") {
                assert_eq!(
                    bundle["captures"]["console"],
                    "missing or empty console capture"
                );
            } else {
                assert!(raw.contains("journal.json.1"));
                assert_eq!(
                    std::fs::read_to_string(dir.join("console.txt"))?,
                    "retained capture"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn unavailable_and_bounded_results() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let path = root.join("results.yaml");
        collect("plan", "vm", &path, root, false, |_| {
            anyhow::bail!("missing command")
        });
        let bundle: Value =
            serde_json::from_str(&read_bounded(&root.join("failure-diagnostics.json"))?)?;
        assert_eq!(bundle["failure"]["tests"][0]["reboot_count"], "unknown");
        assert_eq!(
            bundle["failure"]["tests"][0]["reboot_command_or_step"],
            "unknown"
        );
        std::fs::write(&path, "x".repeat(MAX_BYTES as usize + 1))?;
        assert!(failed_tests(&path).is_err());
        std::fs::write(
            &path,
            "- name: test\n  result: error\n  log: [../outside]\n",
        )?;
        assert_eq!(failed_tests(&path)?["tests"][0]["reboot_count"], "unknown");
        Ok(())
    }
}
