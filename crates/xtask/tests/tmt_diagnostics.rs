//! Exercise the actual run-tmt command, not an injected lifecycle callback.
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

#[test]
fn run_tmt_collects_before_cleanup() -> Result<()> {
    for (failed, preserve, enabled) in [
        (true, false, true),
        (true, true, true),
        (false, false, true),
        (false, true, true),
        (true, false, false),
    ] {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let bin = root.join("bin");
        std::fs::create_dir(&bin)?;
        std::fs::create_dir_all(root.join("tmt/plans"))?;
        std::fs::write(root.join("ADOPTERS.md"), "fixture")?;
        std::fs::write(root.join("tmt/plans/integration.fmf"), "/plan-fixture: {}")?;
        let captures = root.join("captures");
        let scripts = [
            ("rsync", "exit 0"),
            ("df", "exit 0"),
            ("podman", "printf 'centos-10\\n'"),
            (
                "ssh",
                "for arg do last=$arg; done\ncase $last in *whoami) printf 'root\\n';; true) :;; *) printf 'previous boot evidence\\n';; esac",
            ),
            (
                "tmt",
                "case \" $* \" in *' plan ls '*) printf '/plan-fixture\\n';; *' report '*) exit 0;; *) exit \"$TEST_STATUS\";; esac",
            ),
            (
                "bcvk",
                "case $2 in inspect) cat \"$FIXTURE_ROOT/inspect.json\";; rm) if [ \"$EXPECT_COLLECTION\" = 1 ]; then test -s \"$CAPTURES/$5/failure-diagnostics.json\" || exit 99; fi; printf 'cleanup\\n' >> \"$FIXTURE_ROOT/events\";; run) if [ \"${3:-}\" = --help ]; then printf '%s\\n' --log-dir; fi;; esac",
            ),
        ];
        for (name, script) in scripts {
            let file = bin.join(name);
            std::fs::write(&file, format!("#!/bin/sh\nset -eu\n{script}\n"))?;
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700))?;
        }
        let key = [
            "-----BEGIN",
            " OPENSSH PRIVATE",
            " KEY-----\nfixture\n",
            "-----END",
            " OPENSSH PRIVATE",
            " KEY-----",
        ]
        .concat();
        std::fs::write(
            root.join("inspect.json"),
            serde_json::json!({
                "ssh_port": 2222, "ssh_private_key": key, "state": "shut off"
            })
            .to_string(),
        )?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .current_dir(root)
            .args(["run-tmt", "fixture", "--log-dir"])
            .arg(&captures)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE_ROOT", root)
            .env("CAPTURES", &captures)
            .env(
                "BOOTC_TMT_FAILURE_DIAGNOSTICS",
                if enabled { "1" } else { "0" },
            )
            .env("TEST_STATUS", if failed { "1" } else { "0" })
            .env(
                "EXPECT_COLLECTION",
                if failed && enabled { "1" } else { "0" },
            );
        if preserve {
            command.arg("--preserve-vm");
        }
        let output = command.output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), !failed, "{stderr}");
        if failed {
            assert!(stderr.contains("Some test plans failed"), "{stderr}");
        }
        let vm_dir = std::fs::read_dir(&captures)?.next().unwrap()?.path();
        let manifest = vm_dir.join("failure-diagnostics.json");
        assert_eq!(manifest.exists(), failed && enabled, "{stderr}");
        let events = std::fs::read_to_string(root.join("events")).unwrap_or_default();
        assert_eq!(events, if preserve { "" } else { "cleanup\n" }, "{stderr}");
        if manifest.exists() {
            let raw = std::fs::read_to_string(manifest)?;
            assert!(!raw.contains(&key));
            let data: Value = serde_json::from_str(&raw)?;
            assert_eq!(data["plan"], "/plan-fixture");
            assert_eq!(data["failure"]["tests"][0]["reboot_count"], "unknown");
            assert_eq!(
                std::fs::read_to_string(vm_dir.join("previous-boot-journal.txt"))?,
                "previous boot evidence\n"
            );
        }
    }
    Ok(())
}
