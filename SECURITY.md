# Security

## Safe Usage

### Dry-run before every destructive action

Every command that modifies the system supports `--dry-run`:

```bash
mu clean --dry-run      # shows sizes, touches nothing
mu optimize --dry-run   # shows plan, executes nothing
mu uninstall --dry-run  # shows what would be removed
```

`mu status` is read-only and never modifies the system.

`mu audit --report` and `mu audit --json` are read-only. Interactive `mu audit` apply uses the same trash, whitelist, opt-in, and confirmation rules as `mu clean` / `mu optimize` (confirm defaults to NO).

### Confirmation prompt

Commands that delete files require explicit confirmation:

```
Type 'YES' to continue:
```

Any input other than `YES` (case-sensitive) aborts immediately.

---

## Protected Paths

The following paths are blocked at the code level and cannot be deleted or modified by `mu` under any circumstances:

```
/          /boot      /etc       /usr
/lib       /lib64     /bin       /sbin
/proc      /sys       /dev       /run
```

Additionally:
- APT's simulated `autoremove --purge` policy is the only source of package removal candidates. `mu` does not construct kernel purge lists.
- `/var/lib/<package>` paths found during remnant detection are **displayed only** — `mu uninstall` does not attempt to delete them (root-only paths; user is shown the path for manual review).

You can add your own protected paths in `~/.config/mu/config.toml`:

```toml
[protected_paths]
system = ["/data", "/mnt/backup"]
```

---

## Trash, Not Delete

For user-owned files (under `~/.cache`, `~/.config`, `~/.local`), `mu` uses `gio trash` when available, or moves files to `~/.local/share/Trash/files/` following the FreeDesktop Trash specification. Files are **never permanently deleted** with `rm -rf` from user home directories.

For system caches (`/var/cache/apt/archives`), `mu` calls `apt clean` or `journalctl --vacuum-*` — the system package manager's own safe cleanup commands. `mu` never calls `rm -rf /var/...` directly.

---

## Audit Log

Every operation `mu` performs is recorded in:

```
~/.local/share/mu/operations.log
```

Log format: `2006-01-02T15:04:05Z07:00 [op] path`

The log rotates at 10 MB (one `.log.1` backup kept). Set `MU_NO_OPLOG=1` to disable logging entirely.

---

## Known Limitations

- `mu uninstall` uses `sudo apt purge` and `sudo snap remove` — these require sudo privileges and will prompt for a password.
- `mu optimize` apt steps use interactive `sudo apt-get` (password prompt when needed). Failed steps are marked failed, remaining steps continue, and the command returns nonzero after completion.
- `mu clean` with browser cache (`--include=browser-cache`) may trash files from a running browser session. Close the browser first.
- Real APT, Snap, journal, trash, and interrupted-operation behavior must be validated on disposable supported Ubuntu VMs before release.

---

## Responsible Disclosure

Found a security issue? Please open an issue at:

**https://github.com/baroitrung/mu/issues**

For sensitive issues (credential leaks, privilege escalation), use the GitHub private security advisory feature or email the maintainer directly before public disclosure.
