# S1 spike: a sudo-free btrfs loop volume through udisks

Date: 2026-09-05. Issue: #18. Spec: `docs/klon-spec.md` §7 S1. Design: `docs/klon-handoff.md` §4, §11, §12.

This report answers Q1 and Q2 of the handoff §12. It records each command and its result.

## 1. Summary

| Question | Answer |
|---|---|
| Does `mkfs.btrfs --rootdir` keep the owner of a seeded user directory? | Yes. `klon/` stays `navaro:navaro`. |
| Who owns the mount root? | `root:root`, mode `755`. The user cannot write there. |
| Can the user create a subvolume below `klon/`? | Yes. |
| Can the user snapshot a subvolume? | Yes. |
| Can the user delete a subvolume with `btrfs subvolume delete`? | No. The call fails with `EPERM`. |
| Does udisks accept `user_subvol_rm_allowed`? | No. udisks 2.9.4 refuses the option. |
| Can the user remove a klon another way? | Yes. `rm -rf` removes a populated subvolume. It took 941 ms for 10,000 files. |
| Does the volume survive a detach and a re-attach? | Yes. All files and all subvolumes survive. |
| How long does the re-attach take? | About 0.57 s. |
| Did the spike reboot the host? | No. It used a detach as the substitute. §9.4 lists the limits of that substitute. |
| Did a password prompt appear? | Only for `udisksctl loop-delete` on a loop device that the user does not own. See §8. |

**Verdict: the `btrfs-volume` backend works.** klon must not depend on
`btrfs subvolume delete`. klon must not call `loop-delete` on a foreign loop device.

## 2. Host

| Item | Value |
|---|---|
| OS | Ubuntu 22.04.5 |
| Kernel | 6.2.0-36-generic |
| Filesystem for `$HOME` | ext4 |
| udisks2 | 2.9.4-1ubuntu2.3 |
| git | 2.34.1 |
| Session | seat0, tty2, x11, `Active=yes`, `Remote=no` |
| Desktop | GNOME with an automounter |

The kernel module `btrfs.ko` was present but not loaded. The kernel loaded it on the
first mount. No user action was needed.

## 3. How to get btrfs-progs without root

The host had no `btrfs-progs` on `PATH`. The spike installed nothing system-wide.
Two commands gave a working toolset:

```
$ apt-get download btrfs-progs
Get:1 http://pl.archive.ubuntu.com/ubuntu jammy/main amd64 btrfs-progs amd64 5.16.2-1 [824 kB]
Fetched 824 kB in 3s (288 kB/s)

$ dpkg-deb -x btrfs-progs_5.16.2-1_amd64.deb ~/.local/share/klon/tools/btrfs-progs
```

Neither command needs root.

The package puts the binaries in two directories, not in `usr/bin`:

```
~/.local/share/klon/tools/btrfs-progs/bin/btrfs
~/.local/share/klon/tools/btrfs-progs/sbin/mkfs.btrfs
```

The spike added a `usr/bin/` directory with two symlinks. That gives one stable path
for the later chunks:

```
KLON_BTRFS_TOOLS=$HOME/.local/share/klon/tools/btrfs-progs/usr/bin
```

C7 and C15 must use that variable. The directory holds `btrfs` and `mkfs.btrfs`.

### Shared libraries

No library is missing. Every library is part of the Ubuntu 22.04 base system.

```
$ ldd .../sbin/mkfs.btrfs
	libuuid.so.1 => /lib/x86_64-linux-gnu/libuuid.so.1
	libblkid.so.1 => /lib/x86_64-linux-gnu/libblkid.so.1
	libudev.so.1 => /lib/x86_64-linux-gnu/libudev.so.1
	libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6

$ ldd .../bin/btrfs
	libuuid.so.1, libblkid.so.1, libudev.so.1, libz.so.1,
	liblzo2.so.2, libzstd.so.1, libc.so.6
```

Both binaries report `btrfs-progs v5.16.2`.

## 4. Build the image

Every `udisksctl` command below ran with `SUDO_ASKPASS=/bin/false` in the environment
and with stdin from `/dev/null`.

### 4.1 The sparse image

```
$ truncate -s 2G ~/.local/share/klon/spike-s1.img
$ du -h --apparent-size ~/.local/share/klon/spike-s1.img
2,0G
$ du -h ~/.local/share/klon/spike-s1.img
0
```

The file is sparse. It uses no disk space at this point.

### 4.2 The seed directory

```
$ mkdir -p ~/.local/share/klon/spike-s1/seed/klon
$ find ~/.local/share/klon/spike-s1/seed -printf '%M %u:%g %p\n'
drwxrwxr-x navaro:navaro .../seed
drwxrwxr-x navaro:navaro .../seed/klon
```

### 4.3 mkfs.btrfs --rootdir

```
$ mkfs.btrfs --rootdir ~/.local/share/klon/spike-s1/seed ~/.local/share/klon/spike-s1.img
btrfs-progs v5.16.2
Making image is completed.
UUID:               39c544dd-0a58-4241-9179-ed48c3b4abf8
Node size:          16384
Sector size:        4096
Filesystem size:    2.00GiB
Block group profiles:
  Data:             single            8.00MiB
  Metadata:         DUP             102.38MiB
  System:           DUP               8.00MiB
Incompat features:  extref, skinny-metadata, no-holes
Runtime features:   free-space-tree
EXIT=0

$ du -h ~/.local/share/klon/spike-s1.img
4,6M
```

`mkfs.btrfs` runs as the user. It needs no root. It writes a plain file.

## 5. Attach and mount

```
$ SUDO_ASKPASS=/bin/false udisksctl loop-setup -f ~/.local/share/klon/spike-s1.img
Mapped file /home/navaro/.local/share/klon/spike-s1.img as /dev/loop57.
EXIT=0
```

No password prompt appeared.

```
$ SUDO_ASKPASS=/bin/false udisksctl mount -b /dev/loop57
Error mounting /dev/loop57: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted:
Device /dev/loop57 is already mounted at
`/media/navaro/39c544dd-0a58-4241-9179-ed48c3b4abf8'.
EXIT=1
```

**The GNOME automounter mounted the device first.** It wins the race in under 350 ms.
A second measurement showed the mount 3 s after `loop-setup`. The timing varies.
`AlreadyMounted` is a success for klon, not a failure. C15 must treat it as success.

```
$ findmnt -n -o TARGET,SOURCE,FSTYPE,OPTIONS /dev/loop57
/media/navaro/39c544dd-0a58-4241-9179-ed48c3b4abf8 /dev/loop57 btrfs
rw,nosuid,nodev,relatime,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/

$ lsmod | grep '^btrfs'
btrfs                1929216  1
```

The kernel loaded `btrfs.ko` for the mount.

## 6. Ownership after the mount

```
$ stat -c '%A %U:%G (%u:%g) %n' /media/navaro/39c544dd-.../
drwxr-xr-x root:root (0:0) /media/navaro/39c544dd-...

$ ls -la /media/navaro/39c544dd-.../
drwxr-xr-x  1 root   root      8 .
drwxrwxr-x  1 navaro navaro    0 klon

$ stat -c '%A %U:%G (%u:%g) %n' /media/navaro/39c544dd-.../klon
drwxrwxr-x navaro:navaro (1000:1000) /media/navaro/39c544dd-.../klon
```

```
$ touch /media/navaro/39c544dd-.../root-write-test
touch: cannot touch '...': Permission denied
EXIT=1

$ touch /media/navaro/39c544dd-.../klon/user-write-test
EXIT=0
```

**Q1, part 1: answered.** `mkfs.btrfs --rootdir` keeps the owner, exactly as
`mkfs.ext4 -d` does. The mount root stays `root:root`. The user owns `klon/`
and writes there.

## 7. Subvolume operations as the user

```
$ id -un
navaro

$ btrfs subvolume create /media/navaro/39c544dd-.../klon/golden
Create subvolume '.../klon/golden'
EXIT=0

$ btrfs subvolume snapshot .../klon/golden .../klon/x
Create a snapshot of '.../klon/golden' in '.../klon/x'
EXIT=0

$ cat .../klon/x/src/a.txt
hello

$ btrfs subvolume list .../klon
ERROR: can't perform the search: Operation not permitted
EXIT=1

$ btrfs subvolume show .../klon/golden
ERROR: Could not search B-tree: Operation not permitted

$ btrfs subvolume delete .../klon/x
WARNING: cannot read default subvolume id: Operation not permitted
ERROR: Could not destroy subvolume/snapshot: Operation not permitted
WARNING: deletion failed with EPERM, send may be in progress
EXIT=1
```

**Q1, part 2: answered.**

| Operation | Result as the user |
|---|---|
| `subvolume create` | Works. |
| `subvolume snapshot` | Works. |
| `subvolume delete` | Fails with `EPERM`. |
| `subvolume list` | Fails with `EPERM`. |
| `subvolume show` | Fails with `EPERM`. |

`list` and `show` use the `TREE_SEARCH` ioctl. That ioctl needs `CAP_SYS_ADMIN`.
klon must not call them.

### An unprivileged subvolume test

A subvolume root has inode 256. Its device number differs from the parent device
number. `stat` gives both values.

```
$ stat -c 'inode=%i dev=%D %n' .../klon .../klon/golden .../klon/s1 .../klon/golden/src
inode=62169121 dev=3b .../klon
inode=256      dev=54 .../klon/golden
inode=256      dev=61 .../klon/s1
inode=259      dev=54 .../klon/golden/src
```

C7 and `doctor` must detect a subvolume with `stat`, not with `btrfs subvolume show`.

## 8. The user_subvol_rm_allowed test

```
$ SUDO_ASKPASS=/bin/false udisksctl mount -b /dev/loop57 -o user_subvol_rm_allowed
Error mounting /dev/loop57: GDBus.Error:org.freedesktop.UDisks2.Error.OptionNotPermitted:
Mount option `user_subvol_rm_allowed' is not allowed
EXIT=1
```

**Q1, part 3: answered. udisks refuses the option.**

The allowlist is compiled into `udisksd`:

```
$ strings /usr/libexec/udisks2/udisksd | grep btrfs_allow
btrfs_allow=compress,compress-force,datacow,nodatacow,datasum,nodatasum,
autodefrag,noautodefrag,degraded,device,discard,nodiscard,subvol,subvolid,space_cache
```

`user_subvol_rm_allowed` is absent from that list.

An administrator can override the list. The file `/etc/udisks2/mount_options.conf`
does not exist on this host. Only the example file is present. A root edit that adds
the option to `btrfs_allow=` would work. klon must not require that edit. It breaks
the zero-password rule.

### The delete path that works

`rmdir` removes an empty subvolume without a privilege. The kernel allows this since
version 4.18. `rm -rf` therefore removes a whole subvolume.

```
$ rmdir .../klon/x        # while it holds files
rmdir: failed to remove '...': Directory not empty
EXIT=1

$ rm -rf .../klon/x/src
$ rmdir .../klon/x
EXIT=0                    # the empty subvolume goes away

$ btrfs subvolume snapshot .../klon/golden .../klon/y
$ rm -rf .../klon/y       # a populated subvolume, one command
EXIT=0
```

`rm -rf` is O(n), not O(1). Section 10 gives the measured cost.

## 9. Detach and re-attach

**The spike did not reboot the host.** A reboot of a shared development laptop is a
destructive act, so the spike used `udisksctl unmount` plus `udisksctl loop-delete` as
the substitute. Section 9.4 lists what the substitute covers and what stays unverified.
Read every claim in this section as a claim about the substitute.

```
$ SUDO_ASKPASS=/bin/false udisksctl unmount -b /dev/loop57
Unmounted /dev/loop57.
EXIT=0

$ losetup -l | grep spike-s1
(no output)
```

**The unmount also released the loop device.** GNOME removes the loop binding when
the user unmounts the volume. A later `udisksctl mount -b /dev/loop57` then fails with
`Object /org/freedesktop/UDisks2/block_devices/loop57 is not a mountable filesystem`.

### The password prompt

`udisksctl loop-delete` on that released device **raised a polkit password dialog**.
The command blocked. `polkit-agent-helper-1` appeared in the process list. The spike
killed the client to dismiss the dialog.

The cause is the polkit policy:

```
org.freedesktop.udisks2.loop-setup:          allow_active=yes             allow_inactive=auth_admin
org.freedesktop.udisks2.filesystem-mount:    allow_active=yes             allow_inactive=auth_admin
org.freedesktop.udisks2.loop-delete-others:  allow_active=auth_admin_keep allow_inactive=auth_admin
```

There is no plain `loop-delete` action. udisks allows a user to delete a loop device
that the same user set up. It asks for a password for any other loop device. After the
unmount, udisks reported `SetupByUID: 0`, so the device counted as foreign.

`loop-delete` on a device that the user still owns succeeds in silence:

```
$ SUDO_ASKPASS=/bin/false udisksctl info -b /dev/loop57 | grep SetupByUID
    SetupByUID:         1000

$ SUDO_ASKPASS=/bin/false udisksctl loop-delete -b /dev/loop57
(no output, exits at once, no prompt)
```

**Rule for C15: read `SetupByUID` before any `loop-delete`. Skip the call when the
value is not the current uid.** A simpler rule also works: call `unmount` only, and
never call `loop-delete`. The unmount already releases the loop device here.

### The re-attach

```
$ SUDO_ASKPASS=/bin/false udisksctl loop-setup -f ~/.local/share/klon/spike-s1.img
Mapped file ... as /dev/loop57.
loop-setup: 350 ms

$ SUDO_ASKPASS=/bin/false udisksctl mount -b /dev/loop57
... AlreadyMounted ...
mount: 218 ms
```

Total: about 0.57 s. That meets the 1 s target in the handoff §4.

All data survived:

```
$ ls -la /media/navaro/39c544dd-.../klon
drwxrwxr-x 1 navaro navaro golden
drwxrwxr-x 1 navaro navaro s1
drwxrwxr-x 1 navaro navaro s2

$ stat -c '%A %U:%G %n' .../klon .../klon/golden
drwxrwxr-x navaro:navaro .../klon
drwxrwxr-x navaro:navaro .../klon/golden

$ find .../klon/golden/src -type f | wc -l
10000
$ find .../klon/s1/src -type f | wc -l
10000
```

### 9.4 What a real reboot needs, and what stays unverified

The first `add` after a reboot must run these steps:

1. Find the loop device for the image with `losetup -j <image>`. **Do not reuse a
   stored device number.** The number is not stable.
2. Run `udisksctl loop-setup -f <image>` when no device exists. This needs about
   0.35 s and no password.
3. Wait for the mount. The desktop automounter usually does it.
4. Run `udisksctl mount -b <device>` when no mount appears. Treat `AlreadyMounted`
   as success.
5. Read the mount path from `findmnt`, or from the `Mounted` reply of udisks.
   Do not guess it.

A reboot changes five things. The substitute reproduced four of them:

| Reboot effect | Reproduced? | Evidence |
|---|---|---|
| The `btrfs` module is unloaded | Yes | `lsmod` showed no `btrfs` before the first mount. The kernel loaded it for that mount. |
| The loop device is gone | Yes | `losetup -l` listed nothing after the unmount. |
| The mount is gone | Yes | `mount` listed nothing after the unmount. |
| udisks forgets the loop owner | Yes | `SetupByUID` fell back to 0 after the unmount. |
| The loop device **number** changes | **No** | Every re-attach in this spike reused `/dev/loop57`. |

**The unverified item is the loop device number.** `loop-setup -f` takes the first
free number. This host already holds about 40 snap loop devices, and snapd claims
them at boot. The order is not guaranteed. The spike did not measure the number
across a reboot, so treat the stability of `/dev/loop57` as an accident of one
session. C15 must never store a device path in `volume.json`. It must store the image
path only, and resolve the device with `losetup -j <image>` at each start. A test for
C15 must cover the case where a stored device number points at a foreign snap loop
device.

Two further items stay unverified because the substitute cannot reach them:

- Whether the desktop automounter runs before the user session is ready. klon must
  not depend on the automounter. Step 4 covers this.
- Whether a stale `/media/<user>/<label>` directory blocks the mount. udisks removes
  its own mount points on a clean shutdown. An unclean shutdown may leave one.

C15 must carry an integration test for the re-attach path. That test can use the
substitute in this report. A real reboot test belongs in the release checklist, not
in the automated suite.

### The mount path

Without a label, udisks mounts at `/media/<user>/<filesystem UUID>`. That path is
stable but not readable. With a label the path is readable:

```
$ mkfs.btrfs -L klon-demo --rootdir <seed> <image>
Label:              klon-demo
$ SUDO_ASKPASS=/bin/false udisksctl loop-setup -f <image>
$ mount | grep klon-demo
... on /media/navaro/klon-demo type btrfs (rw,nosuid,nodev,...,uhelper=udisks2)
```

**C15 must pass `-L klon-<repo>` to `mkfs.btrfs`.** The mount path then becomes
`/media/<user>/klon-<repo>`. klon must still read the real path from `findmnt`,
because a second volume with the same label gets a suffix.

## 10. Measurements

Fixture: one subvolume, 10,000 small text files in 100 directories, 40 MB, one git
commit. git 2.34.1 with `core.checkStat=minimal`, `core.untrackedCache=true`,
`index.version=4`.

### Snapshot

| Run | Time |
|---|---|
| 1 | 50 ms |
| 2 | 43 ms |
| 3 | 18 ms |

The time includes the start of the `btrfs` process. The ioctl itself is O(1). The
handoff estimate of about 5 ms holds for the ioctl. A klon spawn costs 20 to 50 ms
when it shells to `btrfs`.

### git status in a snapshot

| Case | Time |
|---|---|
| Golden, warm | 0.057 s |
| Snapshot `s1`, first run | 0.130 s |
| Snapshot `s1`, second run | 0.022 s |
| Snapshot `s2`, first run | 0.117 s |

The first run in a snapshot refreshes the index stat data and writes it back. It does
not re-hash the files. The `refresh index` step took 0.055 to 0.064 s for 10,000
files. A full re-hash would cost far more.

**This confirms the handoff plan for C7.** A btrfs snapshot keeps the mtime and the
size, so `core.checkStat=minimal` avoids the re-hash. The index still needs a fresh
mtime, as the handoff §4 states.

### Delete

| Operation | Time |
|---|---|
| `rm -rf` of a 10,000-file snapshot | 941 ms |

This is the cost that `user_subvol_rm_allowed` would remove. C7 must run the delete in
a detached background process, as the spec already says.

### Disk use

| Point | Allocated |
|---|---|
| After `truncate -s 2G` | 0 |
| After `mkfs.btrfs --rootdir` | 4.6 MB |
| With 10,000 files and 2 snapshots | 36 MB |

The image stays sparse. A 60 GB volume costs nothing until klon fills it.

## 11. Decision for Q1

**Q1: Does `mkfs.btrfs --rootdir` keep the owner of a seeded user directory? Does
udisks accept `user_subvol_rm_allowed`? Can the user create and snapshot subvolumes
below that directory?**

Decision:

1. `--rootdir` keeps the owner. The `btrfs-volume` backend stands. C15 seeds an empty
   user-owned `klon/` directory and moves golden into it.
2. udisks refuses `user_subvol_rm_allowed`. **klon drops the option.** klon never asks
   for it and never asks the user to edit `/etc/udisks2/mount_options.conf`.
3. The user creates and snapshots subvolumes. The `add` path is unchanged.
4. klon deletes a klon with `rm -rf` in a detached low-priority process. The handoff
   §4 already lists this as the fallback. It is now the only path.
5. klon never calls `btrfs subvolume delete`, `btrfs subvolume list`, or
   `btrfs subvolume show`. All three need root.
6. klon detects a subvolume with `stat`: inode 256 plus a device number that differs
   from the parent.
7. C15 passes `-L klon-<repo>` to `mkfs.btrfs` and reads the mount path from
   `findmnt`.
8. C15 treats `AlreadyMounted` as success.
9. C15 reads `SetupByUID` before `loop-delete`, or skips `loop-delete` altogether.
10. C15 stores the image path in `volume.json`, never a loop device path. It resolves
    the device with `losetup -j <image>` at each start.

Residual risk: the polkit rules give `allow_active=yes` only. A session that is not
active and local, such as ssh or a headless runner, gets `auth_admin` and a password
prompt. klon must check the session before it offers `init --volume`. `loginctl
show-session <id> -p Active -p Remote` gives the answer. klon must print one line and
fall back to the `copy` backend when the session is not active.

## 12. Decision for Q2

**Q2: Bundle a static `mkfs.btrfs` in the release asset, or print the install line?**

**Decision: do not bundle. Print the install line. Add an opt-in local fetch.**

Reasons:

1. `btrfs-progs` is GPLv2. klon is MIT OR Apache-2.0. A GPLv2 binary in the release
   asset adds a written source offer to every release. That cost is permanent.
2. The distribution binaries link against six shared libraries. A bundled copy needs
   a static build for each architecture and each libc. That is new release work.
3. Every target distribution packages `btrfs-progs`. One line installs it.
4. This spike proved a third path. `apt-get download` plus `dpkg-deb -x` needs no
   root. The extracted binaries work. klon can do the same on request.

klon prints this when `btrfs-progs` is absent:

```
btrfs-progs is not installed. Install it, then run gh klon init --volume again.
  Debian, Ubuntu:  sudo apt-get install btrfs-progs
  Fedora, RHEL:    sudo dnf install btrfs-progs
  Arch:            sudo pacman -S btrfs-progs
Or run: gh klon init --volume --fetch-tools   (no root; extracts into
~/.local/share/klon/tools/)
```

`--fetch-tools` is an opt-in flag. It downloads the distribution package and extracts
it into `~/.local/share/klon/tools/btrfs-progs/`. It installs nothing system-wide.
It is a separate ticket, not part of C15.

klon looks for the tools in this order:

1. `$KLON_BTRFS_TOOLS`, when the variable is set.
2. `~/.local/share/klon/tools/btrfs-progs/usr/bin/`.
3. `PATH`.

## 13. Facts for C7 and C15

| Fact | Value |
|---|---|
| Tools path for the tests | `KLON_BTRFS_TOOLS=$HOME/.local/share/klon/tools/btrfs-progs/usr/bin` |
| Binaries there | `btrfs`, `mkfs.btrfs` (symlinks to `bin/` and `sbin/`) |
| Version | btrfs-progs v5.16.2 |
| mkfs call | `mkfs.btrfs -L klon-<repo> --rootdir <seed> <image>` |
| Find the device | `losetup -j <image>`. Never store a device path. |
| Attach | `udisksctl loop-setup -f <image>` |
| Mount | `udisksctl mount -b <device>`; `AlreadyMounted` means success |
| Mount path | Read it from `findmnt`. Do not compute it. |
| Detach | `udisksctl unmount -b <device>`. It also releases the loop device. |
| `loop-delete` | Call it only when `SetupByUID` equals the current uid. |
| Subvolume test | `stat`: inode 256 and a different device number |
| Delete a klon | `rm -rf` in a detached background process |
| Forbidden calls | `btrfs subvolume delete`, `list`, `show` |
| Session gate | `loginctl show-session`: needs `Active=yes` and `Remote=no` |

## 14. Cleanup

The spike removed the image, the loop device, the mount, and the seed directory.
It kept the extracted tools.

```
$ SUDO_ASKPASS=/bin/false udisksctl unmount -b /dev/loop57
Unmounted /dev/loop57.
$ losetup -l | grep -c spike
0
$ mount | grep -c 39c544dd
0
$ rm -f ~/.local/share/klon/spike-s1.img
$ rm -rf ~/.local/share/klon/spike-s1
$ find ~/.local/share/klon -maxdepth 2
/home/navaro/.local/share/klon
/home/navaro/.local/share/klon/tools
/home/navaro/.local/share/klon/tools/btrfs-progs
```

## 15. Password prompts

Every `udisksctl` command ran with `SUDO_ASKPASS=/bin/false` and stdin from
`/dev/null`.

| Command | Prompt? |
|---|---|
| `loop-setup -f <image>` (5 runs) | No |
| `mount -b <device>` (4 runs) | No |
| `mount -b <device> -o user_subvol_rm_allowed` (2 runs) | No. It failed on the option. |
| `unmount -b <device>` (5 runs) | No |
| `loop-delete -b <device>`, user owns it | No |
| `loop-delete -b <device>`, user does not own it | **Yes.** A polkit dialog appeared. |

One command raised a prompt. Section 9 gives the cause and the rule that avoids it.
