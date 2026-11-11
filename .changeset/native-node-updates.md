---
"fnm": minor
---

Add `fnm update` (alias: `fnm upgrade`) to update installed Node.js versions in place.

Each installed version is compared with the newest release on its own track: stable installs only move to newer stable releases of the same major, and prerelease installs only move within their own channel (`alpha` → newer `alpha`). A prerelease never switches to stable; if a stable release has reached its base version, `fnm update` only reports it. Aliases attached to the old version (`default`, `lts-*`, custom names, ...) are moved to the new one after a successful install.

Old versions are kept unless `--prune` is passed, and `--dry-run` only reports what would happen:

```sh-session
$ fnm update --dry-run
Version      Latest       Aliases      Status
v20.11.0     v20.19.1     default      update available
v22.0.0-rc.1 v22.0.0-rc.2 rc           update available (stable v22.1.0 released)

$ fnm update
Updating v20.11.0 -> v20.19.1
Installing Node v20.19.1 (x64)
  moved alias 'default' -> v20.19.1
Updated 1 version, 1 already up to date.
```

A version can be targeted with `fnm update 20`, `fnm update 20.11`, or `fnm update default`.
