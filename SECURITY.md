# Security Policy

## Supported versions

Security fixes are applied to the latest release. If you are on an older
version, run `mats update install` first.

| Version | Supported |
|---|---|
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please **do not** open a public issue for a security problem.

Report it privately through GitHub's "Report a vulnerability" button on the
repository's Security tab, or by email to the maintainer address listed on the
repository profile. Include:

- the affected MATS version and platform;
- a description of the problem and its impact;
- the smallest reproduction you can provide;
- whether the issue requires a malicious mod JAR, a malicious download server,
  or only local access.

You can expect an acknowledgement within a few days. Please allow time for a fix
and a release before disclosing the issue publicly.

## What counts as a security issue

MATS downloads code and data from the network and executes Minecraft with a mod
loaded, so the following are in scope:

- escaping the extraction directory through archive paths, symlinks or hard
  links;
- executing mod code during `inspect` or `analyze`, which must never happen;
- accepting a download whose checksum does not match, or installing an update
  without a verified checksum;
- overwriting files outside the MATS home, environment or install target;
- command or argument injection when building launch commands;
- privilege escalation through the Windows update helper;
- exposing tokens or credentials in logs, reports or error messages.

## Out of scope

- A mod crashing, misbehaving or containing malicious code. MATS runs real mods
  by design; only run mods you are willing to execute.
- Minecraft itself, Mojang services, loader projects, or third-party mod
  repositories.
- Denial of service caused by a mod consuming all memory or CPU inside its own
  test environment.
- Missing features, incorrect diagnostics or cosmetic problems — those belong in
  the normal issue tracker.

## Safety model

- Mod JARs are inspected without being loaded.
- Downloads are verified against Mojang SHA-1 values or release SHA-256 values
  when published; updates without a checksum are refused.
- Archives are validated before extraction, and links must resolve inside the
  destination.
- Every test runs in its own environment directory and process group.
- MATS never accepts the Minecraft EULA on your behalf.
