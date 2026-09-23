//! End-to-end tests that run the atp binary.
//!
//! Nothing here starts Atmel Studio. Each test works on a copy of the fixture
//! solution in a temporary directory, with a stand-in for the Studio
//! installation, so the suite runs anywhere including in WSL and on a build
//! machine with no Studio at all.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const ATP: &str = env!("CARGO_BIN_EXE_atp");

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temporary directory that cleans itself up.
struct Workspace {
    path: PathBuf,
}

/// Where test workspaces are created.
///
/// Under the crate's own `target` directory rather than the system temporary
/// directory, because in WSL `/tmp` is inside the Linux file system, which
/// Atmel Studio cannot open. Keeping the fixtures on the same drive as the
/// crate means the paths a real build would use are the paths under test.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("atp-tests")
}

impl Workspace {
    /// Copies the fixture solution somewhere writable.
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = workspace_root().join(format!("{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("cannot create the test directory");

        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solution");
        copy_tree(&fixtures, &path);

        // A stand-in for the Studio installation, so --dry-run can name a
        // real file without Studio being installed.
        let studio = path.join("studio");
        fs::create_dir_all(&studio).expect("cannot create the studio directory");
        fs::write(studio.join("AtmelStudio.exe"), b"not a real program")
            .expect("cannot create the studio stand-in");

        Self { path }
    }

    fn solution_directory(&self) -> PathBuf {
        self.path.clone()
    }

    fn project_directory(&self) -> PathBuf {
        self.path.join("app")
    }

    fn studio(&self) -> PathBuf {
        self.path.join("studio")
    }

    /// Creates the files a build would have left behind.
    fn build_artifacts(&self, configuration: &str, extensions: &[&str]) {
        let directory = self.project_directory().join(configuration);
        fs::create_dir_all(&directory).expect("cannot create the output directory");
        for extension in extensions {
            fs::write(directory.join(format!("app.{extension}")), b"artifact")
                .expect("cannot create an artifact");
        }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("cannot create a directory");
        }
        fs::write(path, contents).expect("cannot write a file");
    }

    /// Runs atp in a directory, with the Studio stand-in already pointed at.
    fn run(&self, directory: &Path, arguments: &[&str]) -> Run {
        let output = Command::new(ATP)
            .arg("--studio")
            .arg(self.studio())
            .args(arguments)
            .current_dir(directory)
            .env("NO_COLOR", "1")
            // Keep a real atp.toml in the user's own configuration directory
            // from reaching into the tests.
            .env("XDG_CONFIG_HOME", self.path.join("empty-config"))
            .env("APPDATA", self.path.join("empty-config"))
            .output()
            .expect("cannot run atp");
        Run::new(output)
    }

    fn run_here(&self, arguments: &[&str]) -> Run {
        self.run(&self.solution_directory(), arguments)
    }

    /// Runs a `--dry-run` build, or reports that this checkout cannot.
    ///
    /// Building needs a path a Windows program can open. When the crate lives
    /// in the WSL file system there is no such path, and refusing is the
    /// correct behaviour rather than a defect.
    fn dry_run(&self, arguments: &[&str]) -> Option<Run> {
        let run = self.run_here(arguments);
        if run.stderr.contains("inside the WSL file system") {
            eprintln!(
                "skipped: this checkout is in the WSL file system, which Atmel Studio cannot open"
            );
            return None;
        }
        run.succeeded();
        Some(run)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn new(output: Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn succeeded(&self) -> &Self {
        assert_eq!(
            self.code,
            Some(0),
            "atp failed\nstdout: {}\nstderr: {}",
            self.stdout,
            self.stderr
        );
        self
    }

    fn failed_with(&self, code: i32) -> &Self {
        assert_eq!(
            self.code,
            Some(code),
            "expected exit {code}\nstdout: {}\nstderr: {}",
            self.stdout,
            self.stderr
        );
        self
    }

    fn stdout_has(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle),
            "stdout has no {needle:?}:\n{}",
            self.stdout
        );
        self
    }

    fn stderr_has(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "stderr has no {needle:?}:\n{}",
            self.stderr
        );
        self
    }

    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().map(str::trim).collect()
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("cannot create a directory");
    for entry in fs::read_dir(from).expect("cannot read the fixtures") {
        let entry = entry.expect("cannot read a fixture entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("cannot copy a fixture");
        }
    }
}

#[test]
fn asks_for_a_command_when_given_none() {
    let workspace = Workspace::new();
    workspace
        .run_here(&[])
        .failed_with(2)
        .stderr_has("a command is required");
}

#[test]
fn prints_usage_when_run_with_no_arguments_at_all() {
    let output = Command::new(ATP).output().expect("cannot run atp");
    let run = Run::new(output);
    run.failed_with(2);
    assert!(
        run.stderr.contains("Usage") || run.stdout.contains("Usage"),
        "no usage message:\n{}\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn finds_the_solution_from_inside_the_project_directory() {
    let workspace = Workspace::new();
    let directory = workspace.project_directory();
    workspace
        .run(&directory, &["configs"])
        .succeeded()
        .stdout_has("app (ATSAM4E8C)")
        .stdout_has("Release_to_application_partition");
}

#[test]
fn lists_the_configurations_in_the_solution_order() {
    let workspace = Workspace::new();
    let run = workspace.run_here(&["configs"]);
    run.succeeded();
    assert_eq!(
        run.lines(),
        vec![
            "app (ATSAM4E8C)",
            "Debug",
            "Release_to_application_partition",
            "Release",
        ]
    );
}

#[test]
fn lists_the_projects_in_the_solution() {
    let workspace = Workspace::new();
    workspace
        .run_here(&["projects"])
        .succeeded()
        .stdout_has("app.atsln")
        .stdout_has("app");
}

#[test]
fn refuses_to_guess_between_several_configurations() {
    let workspace = Workspace::new();
    let run = workspace.run_here(&["build"]);
    run.failed_with(2)
        .stderr_has("several configurations")
        .stderr_has("Release_to_application_partition");
}

#[test]
fn names_the_configurations_when_one_is_misspelled() {
    let workspace = Workspace::new();
    workspace
        .run_here(&["build", "-c", "Debugg"])
        .failed_with(2)
        .stderr_has("no configuration named Debugg")
        .stderr_has("Debug, Release_to_application_partition, Release");
}

#[test]
fn matches_a_configuration_whatever_its_case() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    workspace
        .run_here(&["build", "-c", "debug", "-o"])
        .succeeded()
        .stdout_has("app.elf");
}

#[test]
fn builds_the_studio_command_line() {
    let workspace = Workspace::new();
    let Some(run) = workspace.dry_run(&["build", "-c", "Debug", "--dry-run"]) else {
        return;
    };
    run.stdout_has("AtmelStudio.exe")
        .stdout_has("app.atsln")
        .stdout_has("/Build Debug")
        // The log switch is the whole reason a build produces any output.
        .stdout_has("/out")
        .stdout_has("atp-build.log");
}

#[test]
fn passes_extra_arguments_through_to_the_backend() {
    let workspace = Workspace::new();
    if let Some(run) =
        workspace.dry_run(&["build", "-c", "Debug", "--dry-run", "--", "/verbosity:diag"])
    {
        run.stdout_has("/verbosity:diag");
    }
}

#[test]
fn artifact_lookup_rejects_backend_arguments_without_rebuild() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    let run = workspace.run_here(&["build", "-c", "Debug", "-o", "--", "/verbosity:diag"]);
    run.failed_with(2)
        .stderr_has("backend arguments require -f");
    assert!(run.stdout.is_empty());
}

#[test]
fn rebuild_and_clean_use_their_own_switches() {
    let workspace = Workspace::new();
    for flag in ["--rebuild", "-f"] {
        if let Some(run) = workspace.dry_run(&["build", "-c", "Debug", flag, "--dry-run"]) {
            run.stdout_has("/Rebuild Debug");
        }
    }
    if let Some(run) = workspace.dry_run(&["build", "-c", "Debug", "--clean", "--dry-run"]) {
        run.stdout_has("/Clean Debug");
    }
}

#[test]
fn prints_the_artifact_path_for_the_wanted_extension() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf", "bin", "hex"]);

    let run = workspace.run_here(&["build", "-c", "Debug", "-e", "bin"]);
    run.succeeded();
    let printed = run.stdout.trim();
    assert!(printed.ends_with("app.bin"), "printed {printed}");
    assert!(Path::new(printed).is_file(), "{printed} does not exist");
}

#[test]
fn defaults_to_the_linked_image() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf", "bin"]);
    let run = workspace.run_here(&["build", "-c", "Debug", "-o"]);
    run.succeeded();
    assert!(run.stdout.trim().ends_with("app.elf"));
}

#[test]
fn takes_the_project_name_on_either_side_of_the_extension() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    // Project selection works before or after the extension option.
    let first = workspace.run_here(&["build", "-c", "Debug", "-e", "elf", "app"]);
    let second = workspace.run_here(&["build", "-c", "Debug", "app", "-e", "elf"]);
    first.succeeded();
    second.succeeded();
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn prints_the_output_directory_before_it_exists() {
    let workspace = Workspace::new();
    let run = workspace.run_here(&["output", "-c", "release"]);
    run.succeeded();
    let directory = Path::new(run.stdout.trim());
    assert!(directory.is_absolute());
    assert_eq!(directory, workspace.project_directory().join("Release"));
    assert!(!directory.exists());
    assert_eq!(run.stdout.lines().count(), 1);
}

#[test]
fn resolves_relative_output_and_explicit_config_paths_before_output_exists() {
    let workspace = Workspace::new();
    let project = workspace.project_directory().join("app.cproj");
    let text = fs::read_to_string(&project).unwrap().replace(
        r"$(MSBuildProjectDirectory)\$(Configuration)",
        r"artifacts\$(Configuration)",
    );
    workspace.write("app/app.cproj", &text);
    workspace.write(
        "settings/atp.toml",
        "[projects.firmware]\nsolution = '../app.atsln'\nconfiguration = 'Release'\n",
    );

    let run = workspace.run_here(&["--config-file", "settings/atp.toml", "output", "firmware"]);
    run.succeeded();
    let directory = Path::new(run.stdout.trim());
    assert!(directory.is_absolute());
    assert_eq!(
        directory,
        workspace.path.join("settings/../app/artifacts/Release")
    );
    assert!(!directory.exists());
    assert_eq!(run.stdout.lines().count(), 1);
}

#[test]
fn prints_the_directory_when_artifacts_exist() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf", "bin"]);
    let run = workspace.run_here(&["output", "-c", "Debug"]);
    run.succeeded();
    assert_eq!(
        Path::new(run.stdout.trim()),
        workspace.project_directory().join("Debug")
    );
}

#[test]
fn rejects_removed_output_options() {
    let workspace = Workspace::new();
    for arguments in [
        vec!["output", "--all"],
        vec!["output", "--copy", "out"],
        vec!["output", "-e", "elf"],
        vec!["build", "--all"],
        vec!["build", "--copy", "out"],
    ] {
        workspace.run_here(&arguments).failed_with(2);
    }
}

#[test]
fn says_what_was_built_when_the_wanted_file_is_missing() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf", "bin"]);
    let run = workspace.run_here(&["build", "-c", "Debug", "-e", "srec"]);
    run.failed_with(2)
        .stderr_has("produced no .srec file")
        .stderr_has("elf, bin");
    assert!(run.stdout.is_empty());
}

#[test]
fn says_when_nothing_has_been_built_yet() {
    let workspace = Workspace::new();
    let run = workspace.run_here(&["build", "-c", "Debug", "-o"]);
    run.failed_with(1).stderr_has("has not been built yet");
    assert!(run.stdout.is_empty());
}

#[test]
fn defaults_to_elf_even_when_the_project_configures_another_extension() {
    let workspace = Workspace::new();
    let project = workspace.project_directory().join("app.cproj");
    let text = fs::read_to_string(&project).unwrap().replace(
        "<OutputFileExtension>.elf</OutputFileExtension>",
        "<OutputFileExtension>.hex</OutputFileExtension>",
    );
    workspace.write("app/app.cproj", &text);
    workspace.build_artifacts("Debug", &["elf", "hex"]);
    let run = workspace.run_here(&["build", "-c", "Debug", "-o"]);
    run.succeeded();
    assert_eq!(
        Path::new(run.stdout.trim()),
        workspace.project_directory().join("Debug/app.elf")
    );
}

#[test]
fn lookup_does_not_need_a_studio_installation() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    fs::remove_dir_all(workspace.studio()).unwrap();
    workspace
        .run_here(&["build", "-c", "Debug", "-o"])
        .succeeded()
        .stdout_has("app.elf");
}

#[test]
fn rebuild_lookup_dry_run_reserves_stdout_for_the_artifact() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    for flags in [vec!["-fo"], vec!["-f", "-e", "bin"]] {
        let mut arguments = vec!["build", "-c", "Debug", "--dry-run"];
        arguments.extend(flags);
        if let Some(run) = workspace.dry_run(&arguments) {
            assert!(run.stdout.is_empty(), "{}", run.stdout);
            run.stderr_has("/Rebuild Debug")
                .stderr_has("AtmelStudio.exe");
        }
    }
}

#[test]
fn failed_rebuild_never_prints_an_existing_artifact() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    // The Studio stand-in cannot execute, even if the host supports builds.
    let run = workspace.run_here(&["build", "-c", "Debug", "-fo"]);
    run.failed_with(1);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    assert!(!run.stderr.is_empty());
}

#[test]
fn clean_conflicts_with_artifact_lookup() {
    let workspace = Workspace::new();
    for flags in [vec!["-o"], vec!["-e", "elf"]] {
        let mut arguments = vec!["build", "-c", "Debug", "--clean"];
        arguments.extend(flags);
        let run = workspace.run_here(&arguments);
        run.failed_with(2);
        assert!(run.stdout.is_empty());
    }
}

#[test]
fn warns_when_the_artifacts_are_older_than_the_sources() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    // The Windows clock only moves every 15 ms or so, and two writes inside
    // one tick get the same timestamp, which is not what is being tested.
    std::thread::sleep(std::time::Duration::from_millis(50));
    // Touching a source after the build is what makes the output stale.
    workspace.write("app/src/main.c", "int main(void) { return 0; }\n");

    let run = workspace.run_here(&["build", "-c", "Debug", "-o"]);
    run.succeeded()
        .stderr_has("older than the sources")
        // The path still has to be the only thing on stdout.
        .stdout_has("app.elf");
    assert_eq!(run.stdout.lines().count(), 1);
}

#[test]
fn a_named_project_in_the_configuration_file_works_from_anywhere() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Release", &["elf"]);
    let solution = workspace.solution_directory().join("app.atsln");
    workspace.write(
        "elsewhere/atp.toml",
        &format!(
            "[defaults]\nproject = 'firmware'\n\n\
             [projects.firmware]\ndescription = 'the one under test'\n\
             solution = '''{}'''\nconfiguration = 'Release'\n",
            solution.display()
        ),
    );

    let elsewhere = workspace.solution_directory().join("elsewhere");
    workspace
        .run(&elsewhere, &["build", "-o"])
        .succeeded()
        .stdout_has("app.elf");
    workspace
        .run(&elsewhere, &["projects"])
        .succeeded()
        .stdout_has("firmware (default)")
        .stdout_has("the one under test");
}

#[test]
fn a_default_configuration_removes_the_need_for_the_option() {
    let workspace = Workspace::new();
    workspace.build_artifacts("Debug", &["elf"]);
    workspace.write("atp.toml", "[defaults]\nconfiguration = 'Debug'\n");
    workspace
        .run_here(&["build", "-o"])
        .succeeded()
        .stdout_has("app.elf");
    workspace
        .run_here(&["configs"])
        .succeeded()
        .stdout_has("Debug (default)");
}

#[test]
fn reports_a_misspelled_key_in_the_configuration_file() {
    let workspace = Workspace::new();
    workspace.write("atp.toml", "[defaults]\nconfigration = 'Debug'\n");
    workspace
        .run_here(&["configs"])
        .failed_with(2)
        .stderr_has("configration");
}

#[test]
fn reports_a_configuration_file_that_is_not_there() {
    let workspace = Workspace::new();
    workspace
        .run_here(&["--config-file", "nowhere.toml", "configs"])
        .failed_with(2)
        .stderr_has("does not exist");
}

#[test]
fn refuses_to_choose_between_two_solutions() {
    let workspace = Workspace::new();
    workspace.write("second.atsln", "\n");
    workspace
        .run_here(&["configs"])
        .failed_with(2)
        .stderr_has("several .atsln files");
}

#[test]
fn says_so_when_there_is_no_project_at_all() {
    // This directory has to sit outside the fixture, because discovery walks
    // up and would otherwise find the solution above it.
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let empty = env::temp_dir().join(format!("atp-empty-{}-{unique}", std::process::id()));
    let _ = fs::remove_dir_all(&empty);
    fs::create_dir_all(&empty).expect("cannot create the empty directory");

    let output = Command::new(ATP)
        .args(["configs"])
        .current_dir(&empty)
        .env("NO_COLOR", "1")
        .env("XDG_CONFIG_HOME", empty.join("none"))
        .env("APPDATA", empty.join("none"))
        .output()
        .expect("cannot run atp");
    let run = Run::new(output);
    let _ = fs::remove_dir_all(&empty);

    run.failed_with(2).stderr_has("no .atsln or .cproj found");
}

#[test]
fn prints_the_user_configuration_directory() {
    let workspace = Workspace::new();
    workspace
        .run_here(&["--config-dir"])
        .succeeded()
        .stdout_has("atp");
}
