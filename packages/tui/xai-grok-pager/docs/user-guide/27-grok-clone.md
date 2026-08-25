# grok clone

`grok clone` fetches a Git repository into a Grove content store and mounts a
projected working tree (NFS on macOS, FUSE on Linux). It is gated by
`[clone] enabled = true` in Grove config (`~/.config/grove/config.toml`).

```bash
grok clone <url> [dir] [--branch NAME] [--cone PATH]... [--full-history]
```

## History

**Default is a depth-1 bootstrap** of the selected branch (`blob:none` +
`--depth=1`). Only that branch is advertised as a remote-tracking ref.

Use `--full-history` when you need complete commit history, tags, or every
remote branch at clone time (the previous default).

After a depth-1 clone, these commands deepen **only the selected branch**:

```bash
git fetch --deepen=N origin
git fetch --unshallow origin
```

Fetching another branch needs an explicit depth-limited refspec. An ordinary
`git fetch origin` or `git fetch origin other` will not pull that branch's
full history through the default refspec:

```bash
git fetch --depth=1 origin refs/heads/NAME:refs/remotes/origin/NAME
```

A default shallow clone requires a Grove daemon that understands the
`clone_shallow` RPC. If the client refuses, restart or update the daemon (or
pass `--full-history`).
