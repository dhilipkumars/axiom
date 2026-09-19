---
kind: added
---

Releases now publish **`.deb` and `.rpm` packages** beside the tarballs, one
per Postgres major and architecture. Installing into a Postgres you already
run is a package manager command rather than a `cp`:

```sh
# Debian / Ubuntu
sudo apt install ./postgresql-17-axiom_<version>-1_amd64.deb

# RHEL / Rocky / Alma 9, with PGDG's repo enabled
sudo dnf install ./axiom_17-<version>-1.el9.x86_64.rpm
```

Each follows its own convention, so the package looks native on either side:
Debian's `postgresql-<major>-axiom` installing under
`/usr/lib/postgresql/<major>`, and PGDG's `axiom_<major>` under
`/usr/pgsql-<major>`. They own the three extension files and not the
directories, so neither conflicts with the Postgres server package. `apt
remove` and `dnf remove` take Axiom away again, which a tarball never offered.

**The reason to prefer a package is what happens when you are on the wrong
distro.** Both declare the glibc floor the binary actually has, so the package
manager refuses up front and installs nothing:

    nothing provides libc.so.6(GLIBC_2.29)(64bit) needed by axiom_17

Previously that machine accepted every file, and you found out after editing
`shared_preload_libraries` and restarting -- at which point Postgres would not
start and the cause looked like Axiom rather than like a download for the wrong
system.

**The supported floor is glibc 2.34**, which is Debian 12+, Ubuntu 22.04+ and
RHEL 9+. Earlier notes derived this from the build image's glibc and said 2.36,
which wrongly excluded Ubuntu 22.04 and RHEL 9 -- both run Axiom. Debian
bullseye and RHEL 8 are genuinely too old. The floor is now read from the
binary at package time rather than written down, so it cannot drift again.

Tarballs are unchanged and remain the option where no package manager applies.
