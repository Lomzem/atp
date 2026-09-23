//! Running a build and collecting the log.
//!
//! Two backends produce the same report. Atmel Studio is the one that always
//! works, because Atmel's makefile generator needs the Studio shell around it;
//! it is a windowed program, so the only way to see anything is to make it
//! write a log file and read that back. MSBuild is faster and streams its
//! output, but on many installations its makefile generator fails with a null
//! reference, so it stays opt-in.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::host::Paths;
use crate::report::{self, Report};
use crate::solution::Project;
use crate::{AppError, AppResult};

/// Where atp writes the build log, inside the configuration's output directory.
///
/// It has to live somewhere a Windows process can write, which rules out
/// `/tmp` when atp runs in WSL. The output directory is always on a Windows
/// drive, because the project itself has to be.
const LOG_NAME: &str = "atp-build.log";

/// How often the progress line checks the log for a newly compiled file.
const POLL: Duration = Duration::from_millis(200);

/// Studio installations to try when the user has not named one.
const STUDIO_ROOTS: [&str; 4] = [
    "C:\\Program Files (x86)\\Atmel\\Studio\\7.0",
    "C:\\Program Files (x86)\\Microchip\\Studio\\7.0",
    "C:\\Program Files\\Atmel\\Studio\\7.0",
    "C:\\Program Files\\Microchip\\Studio\\7.0",
];

/// The .NET Framework MSBuild that Atmel's targets are written for.
const MSBUILD_PATHS: [&str; 2] = [
    "C:\\Windows\\Microsoft.NET\\Framework64\\v4.0.30319\\MSBuild.exe",
    "C:\\Windows\\Microsoft.NET\\Framework\\v4.0.30319\\MSBuild.exe",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Backend {
    Studio,
    MsBuild,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    Build,
    Rebuild,
    Clean,
}

impl Action {
    fn studio_switch(self) -> &'static str {
        match self {
            Self::Build => "/Build",
            Self::Rebuild => "/Rebuild",
            Self::Clean => "/Clean",
        }
    }

    fn msbuild_target(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Rebuild => "ReBuild",
            Self::Clean => "Clean",
        }
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Build => "Building",
            Self::Rebuild => "Rebuilding",
            Self::Clean => "Cleaning",
        }
    }
}

/// An Atmel Studio installation.
#[derive(Clone, Debug)]
pub(crate) struct Installation {
    root: PathBuf,
}

impl Installation {
    /// Finds Studio, preferring anything the user pointed at.
    pub(crate) fn locate(paths: &Paths, configured: Option<&str>) -> AppResult<Self> {
        let mut candidates = Vec::new();
        if let Some(root) = configured {
            candidates.push(paths.to_native(root)?);
        }
        if let Some(root) = std::env::var_os("ATP_STUDIO_ROOT")
            && !root.is_empty()
        {
            candidates.push(paths.to_native(&root.to_string_lossy())?);
        }
        for root in STUDIO_ROOTS {
            candidates.push(paths.to_native(root)?);
        }

        for root in &candidates {
            if executable(root).is_file() {
                return Ok(Self { root: root.clone() });
            }
        }

        let tried = candidates
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Err(AppError::Runtime(format!(
            "cannot find Atmel Studio. Tried:\n{tried}\n\
             Set its directory with --studio, ATP_STUDIO_ROOT, or [studio] root in atp.toml."
        )))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn studio_executable(&self) -> PathBuf {
        executable(&self.root)
    }
}

fn executable(root: &Path) -> PathBuf {
    root.join("AtmelStudio.exe")
}

/// What to build.
pub(crate) struct Request<'a> {
    pub(crate) solution: &'a Path,
    pub(crate) project: &'a Project,
    /// Passed to Studio only when the solution holds more than one project.
    pub(crate) project_name: Option<&'a str>,
    pub(crate) configuration: &'a str,
    pub(crate) action: Action,
    pub(crate) backend: Backend,
    pub(crate) extra: &'a [String],
}

/// The result of a build, whether or not the compiler was happy.
pub(crate) struct Outcome {
    pub(crate) report: Report,
    pub(crate) log: PathBuf,
    pub(crate) elapsed: Duration,
    pub(crate) exit_code: Option<i32>,
}

/// Builds the argument list, which `--dry-run` prints instead of running.
pub(crate) fn command_line(
    request: &Request<'_>,
    paths: &Paths,
    installation: &Installation,
) -> AppResult<Vec<String>> {
    let log = log_path(request);
    match request.backend {
        Backend::Studio => {
            let mut argv = vec![
                paths.to_windows(&installation.studio_executable())?,
                paths.to_windows(request.solution)?,
                request.action.studio_switch().to_string(),
                request.configuration.to_string(),
            ];
            if let Some(name) = request.project_name {
                argv.push("/project".into());
                argv.push(name.to_string());
            }
            argv.push("/out".into());
            argv.push(paths.to_windows(&log)?);
            argv.extend(request.extra.iter().cloned());
            Ok(argv)
        }
        Backend::MsBuild => {
            let mut argv = vec![
                paths.to_windows(&msbuild(paths)?)?,
                paths.to_windows(&request.project.path)?,
                format!("/p:Configuration={}", request.configuration),
                format!("/t:{}", request.action.msbuild_target()),
                "/nologo".into(),
                "/v:normal".into(),
            ];
            argv.extend(request.extra.iter().cloned());
            Ok(argv)
        }
    }
}

/// Runs the build and reads the log back.
pub(crate) fn run(
    request: &Request<'_>,
    paths: &Paths,
    installation: &Installation,
    quiet: bool,
) -> AppResult<Outcome> {
    paths.require_windows_programs()?;

    let output_directory = request.project.output_directory(request.configuration);
    // Atmel's pre-build step runs with its working directory set to the output
    // directory, so the build fails outright when it does not exist yet.
    fs::create_dir_all(&output_directory).map_err(|error| {
        AppError::Runtime(format!(
            "cannot create {}: {error}",
            output_directory.display()
        ))
    })?;

    let log = log_path(request);
    let _ = fs::remove_file(&log);

    let argv = command_line(request, paths, installation)?;
    let (program, arguments) = argv.split_first().expect("a command always has a program");

    let mut command = Command::new(native_program(paths, program));
    command.args(arguments);
    command.current_dir(working_directory(request));
    if request.backend == Backend::MsBuild {
        // Atmel's targets import their tasks through this variable, which
        // Studio normally sets for its own process.
        command.env("AVRSTUDIO_EXE_PATH", installation.root());
    }

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| AppError::Runtime(format!("cannot run {}: {error}", program)))?;

    let exit_code = match request.backend {
        Backend::Studio => watch(&mut child, &log, quiet)?,
        Backend::MsBuild => {
            let output = child
                .wait_with_output()
                .map_err(|error| AppError::Runtime(format!("cannot wait for MSBuild: {error}")))?;
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            fs::write(&log, &text).map_err(|error| {
                AppError::Runtime(format!("cannot write {}: {error}", log.display()))
            })?;
            output.status.code()
        }
    };

    let elapsed = started.elapsed();
    let text = fs::read_to_string(&log).unwrap_or_default();
    if text.is_empty() {
        return Err(AppError::Runtime(format!(
            "the build produced no log at {}.\nExit code was {}.",
            log.display(),
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".into())
        )));
    }

    let directory = paths.to_windows(&output_directory).ok();
    let report = report::parse(&text, directory.as_deref());
    Ok(Outcome {
        report,
        log,
        elapsed,
        exit_code,
    })
}

/// Waits for Studio, showing the file it is compiling as the log grows.
fn watch(child: &mut std::process::Child, log: &Path, quiet: bool) -> AppResult<Option<i32>> {
    let show_progress = !quiet && io::stderr().is_terminal();
    let mut last = String::new();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if show_progress && !last.is_empty() {
                    eprint!("\r\x1b[2K");
                    let _ = io::stderr().flush();
                }
                return Ok(status.code());
            }
            Ok(None) => {}
            Err(error) => {
                return Err(AppError::Runtime(format!(
                    "cannot wait for Atmel Studio: {error}"
                )));
            }
        }

        if show_progress
            && let Some(current) = current_file(log)
            && current != last
        {
            eprint!(
                "\r\x1b[2K{} {}",
                report::label("Compiling"),
                truncate(&current)
            );
            let _ = io::stderr().flush();
            last = current;
        }
        thread::sleep(POLL);
    }
}

/// The file Studio most recently started compiling, read from the partial log.
fn current_file(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    text.lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Building file: "))
        .map(|file| file.trim().to_string())
}

fn truncate(text: &str) -> String {
    const WIDTH: usize = 70;
    if text.chars().count() <= WIDTH {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(WIDTH - 3))
        .collect();
    format!("...{tail}")
}

fn log_path(request: &Request<'_>) -> PathBuf {
    request
        .project
        .output_directory(request.configuration)
        .join(LOG_NAME)
}

/// The directory the build runs in.
///
/// Atmel's generated makefile uses paths relative to the project, so the
/// solution directory is what Studio itself uses.
fn working_directory(request: &Request<'_>) -> PathBuf {
    request
        .solution
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Turns the Windows program path back into one this host can execute.
///
/// Under WSL the executable is reached through `/mnt/c`, and interop starts
/// the Windows process from there.
fn native_program(paths: &Paths, program: &str) -> PathBuf {
    paths
        .to_native(program)
        .unwrap_or_else(|_| PathBuf::from(program))
}

fn msbuild(paths: &Paths) -> AppResult<PathBuf> {
    for candidate in MSBUILD_PATHS {
        let path = paths.to_native(candidate)?;
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(AppError::Runtime(
        "cannot find the .NET Framework MSBuild that Atmel's targets need.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_actions_onto_both_backends() {
        assert_eq!(Action::Build.studio_switch(), "/Build");
        assert_eq!(Action::Rebuild.studio_switch(), "/Rebuild");
        assert_eq!(Action::Clean.msbuild_target(), "Clean");
        assert_eq!(Action::Rebuild.verb(), "Rebuilding");
    }

    #[test]
    fn shortens_a_long_progress_line_from_the_left() {
        let long = "a".repeat(200);
        let shown = truncate(&long);
        assert_eq!(shown.chars().count(), 70);
        assert!(shown.starts_with("..."));
        assert_eq!(truncate("short"), "short");
    }
}
