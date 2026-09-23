# atp

`atp` builds Atmel Studio projects from the command line, on Windows or from
inside WSL, and tells you what the compiler said.

Atmel Studio can build from the command line already, but it is a windowed
program: it writes nothing to the console, so a scripted build gives you an
exit code and nothing else. `atp` always builds with a log file, reads the log
back, and prints the errors, warnings and memory usage.

With `atp`, you can:

- Build any configuration of any project in a solution.
- See errors and warnings with file, line and column, without the log noise.
- Get the path of a build artifact, ready to pipe into another tool.
- Run all of that from WSL against a project on a Windows drive.

## Install

You need Atmel Studio 7 (or Microchip Studio 7) installed on Windows.

**Windows, x64:** rename `atp-0.1.0-windows-x86_64.exe` to `atp.exe` and add
its directory to `PATH`.

**WSL:** copy the Linux `atp` binary onto your `PATH` inside the distribution.
It reaches Studio through WSL's Windows interop, so the project has to live on
a Windows drive such as `/mnt/c`. See [Using atp from WSL](#using-atp-from-wsl).

To build from source, see [Build from source](#build-from-source).

## First use

Run `atp` from anywhere inside a project directory. It looks upwards for the
`.atsln` file, so there is nothing to configure:

```text
atp configs
```

```text
app (ATSAM4E8C)
    Debug
    Release_to_application_partition
    Release
```

`atp` reads the solution and the `.cproj`, so it already knows the projects,
the configuration names, the device, and where the output goes.

## Build

```text
atp build --configuration Debug
```

```text
  Building  app [Debug]
   warning  src/main.c:295:6: unused variable 'count' [-Wunused-variable]
  Finished  app [Debug] in 25.1s — 0 errors, 1 warning
    Memory  Program 135,656 B (25.9% full) · Data 63,048 B (48.1% full)
    Output  Debug/app.elf (2.2 MB)
```

When it fails, the exit code is 1 and the reason is at the top:

```text
  Building  app [Debug]
   warning  src/main.c:295:6: unused variable 'unused' [-Wunused-variable]
     error  src/main.c:301:9: implicit declaration of function 'send' [-Werror=implicit-function-declaration]
      note  cc1.exe: some warnings being treated as errors
    Failed  app [Debug] in 6.4s — 1 error, 4 warnings
       Log  Debug/atp-build.log
```

Messages about the generated makefile and the linker command line are hidden,
because they say nothing about your code. `--verbose` shows the whole log
including those. Warnings past the first twenty-five are counted rather than
listed, again unless `--verbose` is given.

| Task | Command |
| --- | --- |
| Build a configuration | `atp build -c Debug` |
| Build every file again | `atp build -c Debug -f` |
| Delete the build output | `atp build -c Debug --clean` |
| Show the whole build log | `atp build -c Debug --verbose` |
| Print the command instead of running it | `atp build -c Debug --dry-run` |
| Pass extra arguments to the backend | `atp build -c Debug -- /verbosity:diag` |

`b` is an alias for `build`, so `atp b -c release -fo` also works.

`atp` writes its log to `atp-build.log` in the configuration's output
directory, and leaves it there for you to read.

## Get an artifact or its directory

Build once, then use `-o` to print the absolute path of the existing `.elf`:

```text
atp build -c Release
atp build -c Release -o
```

```text
C:\work\app\app\Release\app.elf
```

`-e` selects another extension and implies artifact lookup. Lookup does not
run a build. Add `-f`, an alias for `--rebuild`, to rebuild first:

| Task | Command |
| --- | --- |
| Print the existing `.elf` path | `atp build -c Release -o` |
| Print the existing `.bin` path | `atp build -c Release -e bin` |
| Rebuild and print the `.elf` path | `atp build -c Release -fo` |
| Rebuild and print the `.bin` path | `atp build -c Release -f -e bin` |
| Name the project and configuration | `atp build app -c Release -o` |
| Print the artifacts directory | `atp output -c Release` |

With `-o` or `-e`, stdout contains only the artifact path. Rebuild logs go
to stderr, so the command works in a pipeline:

```text
jog flash "$(atp build -c Release -fo)"
```

`-o` defaults to `.elf`, even if the project configures another executable
extension. Missing artifacts and failed rebuilds exit nonzero and print no
path. If existing artifacts are older than the sources, lookup prints the
path and warns on stderr. With `-fo --dry-run`, the command goes to stderr
and stdout stays empty.

`atp output` prints the absolute artifacts directory, even before it exists.
The old `output -e`, `--all`, and `--copy` options are no longer supported.

## Choosing a project and a configuration

Both can come from four places. The later ones win:

1. The `.atsln` and `.cproj` files.
2. `[defaults]` in `atp.toml`.
3. `[projects.<name>]` in `atp.toml`.
4. The command line.

`atp` never guesses a configuration. When a solution offers several and you
have not chosen one, it lists them:

```text
error: app offers several configurations, so choose one with --configuration:
  Debug
  Release_to_application_partition
  Release
Set a default with [defaults] configuration in atp.toml.
```

## atp.toml

The file is optional. It exists to give solutions short names, to set
defaults, and to point at a Studio installed somewhere unusual.

```toml
[defaults]
project = 'firmware'
configuration = 'Release_to_application_partition'

[studio]
root = 'C:\Program Files (x86)\Atmel\Studio\7.0'   # found automatically otherwise

[projects.firmware]
description = 'Main application'
solution = 'C:\work\app\app.atsln'
# project = 'app'              # only when the solution holds several
# configuration = 'Debug'      # a default for this project alone
```

- `firmware` is a name that you choose. `atp build firmware` then works from
  any directory.
- Paths may be written in Windows or Linux form. `atp` converts them to
  whichever the host needs, so the same file works from Windows and from WSL.
- Relative paths start from the directory that holds `atp.toml`.
- Use single quotes around Windows paths, so the backslashes stay as written.

Run `atp projects` to see the names.

`--config-file` selects a file, which must then exist. Without it, `atp` uses
the first file found in this order:

1. `atp.toml` in the current directory.
2. `atp.toml` beside the `atp` executable.
3. `$XDG_CONFIG_HOME/atp/atp.toml`, if that variable is set.
4. `$HOME/.config/atp/atp.toml`, or `%APPDATA%\atp\atp.toml` on Windows.

To print the user configuration directory and exit, run `atp --config-dir`.

## Using atp from WSL

`atp` runs the same Windows programs from WSL through interop, and translates
every path it passes to them. Paths it prints come back in Linux form:

```text
$ atp build -c Debug -o
/mnt/c/work/app/app/Debug/app.elf
```

Two requirements:

**The project must sit on a Windows drive.** Atmel Studio cannot open a path
inside the WSL file system. `atp` says so rather than failing obscurely:

```text
error: /home/dev/app.atsln is inside the WSL file system, which Atmel Studio
cannot open. Move the project onto a Windows drive, such as /mnt/c, and try again.
```

**Windows interop must be enabled.** Some distributions, Arch among them, lose
the `WSLInterop` binfmt handler when systemd takes over binfmt registration.
Every Windows program then fails with `Exec format error`. Check with:

```text
cat /proc/sys/fs/binfmt_misc/WSLInterop
```

The first line should be `enabled`. If the file is missing, put

```ini
[interop]
enabled = true
```

in `/etc/wsl.conf`, run `sudo systemctl restart systemd-binfmt`, and if it
still fails run `wsl --shutdown` in Windows and start the distribution again.

Reading commands such as `atp output` and `atp configs` work without interop,
because they only read files.

## Backends

`atp build` drives Atmel Studio by default:

```text
AtmelStudio.exe app.atsln /Build Debug /out ...\atp-build.log
```

`--backend msbuild` runs MSBuild on the `.cproj` instead, with
`AVRSTUDIO_EXE_PATH` set so that Atmel's targets can find their tasks. It is
faster and streams its output, but Atmel's makefile generator often fails
under a bare MSBuild with `Object reference not set to an instance of an
object`, because it expects the Studio shell around it. Treat it as an
optimisation to try, not something to rely on.

Set a backend for good with `backend = 'msbuild'` under `[defaults]`.

Two things worth knowing about the Studio backend:

- It cannot build a bare `.cproj`. Without a solution it fails with
  `Value cannot be null. Parameter name: url`, so given a project file `atp`
  finds the solution that owns it.
- Its exit code is unreliable: it exits 0 when a project fails to load and
  nothing is built. `atp` decides success from the log, and says so when the
  two disagree.

## Build from source

```text
cargo build --release
cargo test
```

The tests need neither Atmel Studio nor hardware. They run on Windows and in
WSL, and cover the log parser against real build logs kept in
`tests/fixtures/logs`.

## CI and releases

The `Build` GitHub Actions workflow tests and builds the Windows x64 executable
on every push and pull request. You can also start it manually. Download the
`atp-windows-x86_64` artifact from a successful run and extract the `.exe`.

Release commands require `just`, Bash, Git, and an authenticated GitHub CLI
with access to the repository. Run them from WSL or Git Bash on Windows.

1. Update the version in `Cargo.toml`, run `cargo check` to update `Cargo.lock`,
   and commit the changes.
2. Run `just tag` to create and push an annotated `v<version>` tag for `HEAD`.
   The working tree must be clean.
3. Run `just release` to wait for the tag's build and publish a GitHub release
   with `atp-<version>-windows-x86_64.exe` and generated release notes.

Use `just tag v0.1.0` to specify the tag. It must match `Cargo.toml`.
`just tag --force` replaces the local and remote tag.
Use `just release v0.1.0` to release an existing tag, or
`just release v0.1.0 RUN_ID` to choose a successful push or manual build of
that tag's commit. Run `just` to list the commands.
