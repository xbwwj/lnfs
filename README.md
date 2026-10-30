# lnfs

A daemonless fs tiering solution that use manual symlinks.

## Usage

```sh
lnfs $path      # move path to cold disk and symlink back
lnfs -D $path   # the reverse
```
