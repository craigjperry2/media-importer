# Security Policy

## Known Issues

### Attacker-Writable Source Trees

The importer assumes the configured source directory is trusted by the user
running the command. If another user or process can modify the source tree during
an import, there is a time-of-check/time-of-use race between directory scanning,
metadata checks, and later file opening.

In particular, a regular file observed during scanning could be replaced with a
symlink before it is opened for hashing. The scanner is configured not to follow
symlink entries, but later path-based filesystem operations can still follow a
path that changed after scanning.

This project does not treat attacker-writable source paths as an attack vector.
Do not run imports against source trees that untrusted users can modify. Future
hardening could use no-follow/openat-style file opening and inode/device checks
around already-open file handles, but that is outside the current project scope.
