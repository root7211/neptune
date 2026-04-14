# Changelog

## Unreleased

Nothing yet.

---

## v0.1.0 (2026-04-15)

Initial public release.

This is the first version I'd call "actually usable" — the install/resolve loop is solid enough that I've been using it on real projects without it blowing up. That said, it's still v0.1, so expect rough edges.

### What works

- `npt install` resolves the full dependency graph, including transitive `path` dependencies. It writes a `neptune.lock` and sets up symlinks under `nelua_modules/`.
- `npt install --frozen` refuses to touch the lockfile, which is what you want in CI.
- `npt install --force` nukes the lockfile and re-resolves from scratch.
- `npt tree` prints the dependency graph so you can see what's actually being pulled in.
- `npt run` runs a command with the right paths set up.
- `npt doctor` checks that `nelua`, `git`, and a C compiler are on your PATH.
- Conflict detection: if two different paths pull in the same package from different sources, `npt install` will tell you instead of silently picking one.

### What doesn't work yet

- No registry. Only `path` and `git` dependencies for now.
- Git dependencies don't recursively resolve their own deps. If `lib-a` has a `git` dependency on `lib-b`, you'll need to add `lib-b` to your own manifest manually.
- No `npt publish`, no `npt login`, no central index.

### Internal notes

The codebase went through a few rounds of cleanup before this release. The main things that got fixed along the way:

- Git dependencies now get a real `content_sha256` (the HEAD commit hash) written into the lockfile. Previously it was an empty string, which made `--frozen` mode useless for Git deps.
- `generate_modules_mapping` now uses the same `pkg_id` formula as `materialize_pkg` for Git packages (`git-<first 8 chars of commit>`). Before, it was guessing the install directory by taking the first subdirectory alphabetically, which was fragile.
- The resolver's dedup logic now uses `(name, source)` as the key instead of just `name`. Same-named packages from different sources are now a conflict rather than a silent drop.
- Conflict detection actually works for `path` and `git` deps now. The old version tried to parse their source strings as semver version requirements, which always failed, so conflicts were never reported.
- `topological_sort` was doing O(n^2) work. Rewrote it with a proper adjacency list; it's O(V+E) now.
- `atomic_write` now fsyncs the parent directory after the rename, not just the file itself.
- Windows: `symlink_dir` now tries symlink -> junction point -> directory copy, in that order, so it works without admin rights.
