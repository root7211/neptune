# Neptune

A package manager for [Nelua](https://nelua.io), written in Rust.

The command-line tool is called `npt`. The goal is something close to what Cargo does for Rust — declare your dependencies, run `npt install`, and get on with writing code.

This is still early-stage software. The core install/resolve loop works, but there are rough edges.

---

## Installation

You'll need a Rust toolchain (stable) and Git.

```bash
git clone https://github.com/root7211/neptune.git
cd neptune
cargo build --release
cp target/release/npt ~/.local/bin/
```

---

## Getting started

Create a `neptune.toml` in your project root:

```toml
name = "my-project"
version = "0.1.0"

[entry]
app = "src/main.nelua"

[dependencies]
some-lib = { path = "../some-lib" }
another-lib = { git = "https://github.com/someone/another-lib.git", branch = "main" }
```

Then run:

```bash
npt install
```

This resolves the full dependency graph (including transitive deps), clones any Git repos, and sets up symlinks under `nelua_modules/`. A `neptune.lock` file is written so subsequent installs are reproducible.

---

## Commands

| Command | What it does |
|---------|-------------|
| `npt init` | Scaffold a new `neptune.toml` in the current directory |
| `npt install` | Resolve and install all dependencies |
| `npt install --frozen` | Install without modifying the lockfile (good for CI) |
| `npt install --force` | Ignore the existing lockfile and re-resolve from scratch |
| `npt tree` | Print the dependency graph |
| `npt run` | Run a command with `nelua_modules` on the path |
| `npt doctor` | Check that `nelua`, `git`, and a C compiler are available |

---

## Project layout

After `npt install`, your project will look roughly like this:

```
my-project/
├── neptune.toml
├── neptune.lock
├── nelua_modules/
│   ├── some-lib -> .neptune/pkgs/some-lib/path-a1b2c3d4
│   └── another-lib -> .neptune/pkgs/another-lib/git-deadbeef
└── .neptune/
    └── pkgs/
        ├── some-lib/
        └── another-lib/
```

Dependencies are stored under `.neptune/pkgs/` and identified by a content hash, so changing a dependency's source automatically invalidates the cached copy.

---

## Manifest format

```toml
name = "my-lib"          # required, lowercase letters/digits/hyphens only
version = "0.1.0"        # semver

[entry]
lib = "src/lib.nelua"    # use `app` for executables, `lib` for libraries

[dependencies]
# path dependency
foo = { path = "../foo" }

# git dependency — pick one of rev, tag, or branch
bar = { git = "https://github.com/someone/bar.git", tag = "v1.2.0" }
```

---

## Lockfile

`neptune.lock` is generated automatically. Commit it to version control if you're building an application; you can leave it out of `.gitignore` for libraries too, but it's less important there.

The `--frozen` flag makes `npt install` refuse to update the lockfile — useful in CI to catch cases where someone forgot to commit an updated lock.

---

## Known limitations

- Registry support (i.e., a central package index) is not implemented yet. Only `path` and `git` dependencies work.
- Git dependencies don't recursively resolve their own `neptune.toml` deps yet. That's the next thing on the list.
- No `npt publish` or `npt login` yet.
- Windows symlink support requires Developer Mode or admin privileges; Neptune falls back to directory junctions and then plain copies.

---

## Roadmap

- **v0.2** — registry prototype, `npt publish`
- **v0.3** — recursive Git dep resolution, better error messages
- **v0.4** — C dependency integration via `build.nelua`

(Version numbers here refer to future public releases, not the internal development versions in the git history.)

---

## Contributing

Issues and PRs are welcome. There's no formal contribution guide yet — just open an issue if something is broken or confusing.

## License

MIT
