# No hardcoded people, machines or deployments

This repo is public. Anyone can clone it and run `install.sh`, and the result
must fit **their** system: their user, their home directory, their OS, their
email, their hosts. Nothing in code may assume it runs for the person who wrote
the line.

## The rule

Runtime code (anything shipped, installed, served, or sent to a lane) must not
contain:

- **a person's name or handle** used as the owner, the human, an actor or an
  approver (`Ethan`, a GitHub login, …);
- **an email address or mail domain** of a person or company;
- **a home directory or username** (`/Users/<name>`, `/home/<name>`), or a
  path that only exists on the author's machine;
- **a hostname, LAN/tailnet address or port** of a specific deployment;
- **an OS assumption without a fallback**: macOS-only tools (`launchctl`,
  `/private/tmp`, `~/Library`) on a path that also runs on Linux, and the reverse.

## Where the value comes from instead

| Need | Source |
|---|---|
| owner's display name | `settings::owner_name()`: `AMUX_OWNER_NAME` in `~/.amux/server.env`, then `git config user.name`, then `$USER` |
| owner's email | `AMUX_OWNER_EMAIL`, else the connected account; otherwise say it is unset |
| internal mail domains | `AMUX_INTERNAL_EMAIL_DOMAINS` (empty by default) |
| home, user, uid | `$HOME`, `getpwuid(geteuid())`, `id -un`; never a literal fallback path |
| temp dirs | `std::env::temp_dir()` / `$TMPDIR` |
| service manager | pick by OS: `launchctl` on macOS, `systemctl --user` on Linux |
| hosts, ports, URLs | a `server.env` key with a documented generic default, or `endpoint.json` |

Read server config with `effective_env()` at use, not once at startup, so a
settings change takes effect without a restart. In prose sent to lanes, say
"the owner" (or the configured name). Never name a person.

Comments may cite history ("Ethan, 2026-08-13: …"), but keep them rare.
Test fixtures may use example values; prefer `example.com` and obviously fake
names.

## Enforcement

`scripts/test-no-hardcoded-specifics.py` runs in CI. It scans runtime files
(it skips comments, `#[cfg(test)]` modules and test files) for names, emails,
home paths, handles and LAN addresses. Existing violations are recorded in
`scripts/fixtures/hardcoded-specifics-baseline.txt`, and that list is a ratchet:

- a new violation fails CI; move the value into configuration instead;
- fixing one fails CI until you lower that file's count in the baseline, so
  the slack cannot be reused;
- never raise a count or add a file to the baseline to get green.
