# NAME

bootc-fetch-apply-updates.service

# DESCRIPTION

This service causes `bootc` to perform the following steps:

- Check the source registry for an updated container image
- If one is found, download it
- Reboot

The reboot honors systemd inhibitor locks: if a process holds a
`block` mode `shutdown` inhibitor (see `systemd-inhibit(1)`), the reboot
is refused and the service fails. The update remains staged, and is
applied on the next reboot or the next run of this service. To reboot
anyway, use `systemctl reboot --check-inhibitors=no`. Logged-in users
do not prevent the reboot.

This service also comes with a companion `bootc-fetch-apply-updates.timer`
systemd unit.  The current default systemd timer shipped in the upstream
project is enabled for daily updates.

However, it is fully expected that different operating systems
and distributions choose different defaults.

# CUSTOMIZING UPDATES

Note that all three of these steps can be decoupled; they
are:

- `bootc upgrade --check`
- `bootc upgrade`
- `bootc upgrade --apply`

# SEE ALSO

**bootc(1)**

# VERSION

<!-- VERSION PLACEHOLDER -->