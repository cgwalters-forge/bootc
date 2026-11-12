# Verify that bootc-destructive-cleanup.service did its job after an
# in-place reinstall (`system-reinstall-bootc`, which passes `--cleanup`).
#
# A unit that never ran, or whose steps were all skipped, is not "failed",
# so 014-test-no-failed-units.nu alone does not notice a cleanup that
# didn't happen. See https://github.com/bootc-dev/bootc/issues/2559
use std assert
use tap.nu

tap begin "verify destructive cleanup after an in-place reinstall"

const unit = "bootc-destructive-cleanup.service"
# Where the script looks for the previous system's packages
const old_root = "/sysroot"
const rpmdb = "/usr/lib/sysimage/rpm"
# The unit is not ordered before the tests, so allow it to finish
const wait_limit = 5min
const wait_step = 5sec

# Only `cargo xtask run-tmt --provision=reinstall` sets this; the other
# provisioning paths install to a fresh disk, where there is nothing to clean up.
let reinstalled = ($env.BOOTC_REINSTALL_IMAGE? | default "" | is-not-empty)

if not $reinstalled {
    print "# skip: not provisioned via in-place reinstall (BOOTC_REINSTALL_IMAGE not set)"
} else {
    # A oneshot unit without RemainAfterExit goes back to "inactive" when
    # done, which is also its state before it starts; its main process
    # having an exit timestamp tells the two apart.
    mut waited = 0sec
    loop {
        let state = (systemctl show -P ActiveState $unit | str trim)
        let exited = (systemctl show -P ExecMainExitTimestampMonotonic $unit | str trim)
        if $state == "failed" or ($state == "inactive" and $exited != "0") {
            break
        }
        assert ($waited < $wait_limit) $"($unit) did not run to completion within ($wait_limit): ActiveState=($state)"
        sleep $wait_step
        $waited = $waited + $wait_step
    }

    let result = (systemctl show -P Result $unit | str trim)
    let status = (systemctl show -P ExecMainStatus $unit | str trim)
    print $"($unit): Result=($result) ExecMainStatus=($status)"
    assert equal $result "success" $"($unit) did not succeed"
    assert equal $status "0" $"($unit) exited with a non-zero status"

    let old_rpmdb = $"($old_root)($rpmdb)"
    if not ($old_rpmdb | path exists) {
        print $"No rpmdb at ($old_rpmdb): no packages of the previous system remain"
    } else {
        let query = (rpm -qa $"--root=($old_root)" $"--dbpath=($rpmdb)" | complete)
        assert equal $query.exit_code 0 $"rpm -qa on ($old_root) failed: ($query.stderr)"
        let packages = ($query.stdout | lines | where $it != "")
        print $"Packages of the previous system left in ($old_root): ($packages | length)"
        assert equal ($packages | length) 0 $"Previous system's packages were not removed: ($packages | first 10 | str join ' ')"
    }
}

tap ok
