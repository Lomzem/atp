//! The command line.

use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use clap::{Parser, Subcommand, ValueEnum};

use crate::builder::{self, Action, Backend, Installation, Request};
use crate::config::{self, Config};
use crate::host::Paths;
use crate::report::{self, Style, file_size, label, relative_to, thousands};
use crate::solution::{self, Project, Solution};
use crate::{AppError, AppResult};

#[derive(Parser)]
#[command(
    name = "atp",
    version,
    about = "Build Atmel Studio projects from Windows or WSL",
    arg_required_else_help = true
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help = "Configuration file to use"
    )]
    config_file: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        help = "Print the user configuration directory and exit"
    )]
    config_dir: bool,

    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help = "Atmel Studio installation directory"
    )]
    studio: Option<String>,

    #[arg(
        long,
        global = true,
        value_enum,
        help = "Build backend [default: studio]"
    )]
    backend: Option<BackendChoice>,

    #[arg(short = 'v', long, global = true, help = "Show the whole build log")]
    verbose: bool,

    #[arg(short = 'q', long, global = true, help = "Print only what failed")]
    quiet: bool,

    #[arg(long, global = true, help = "Print the command instead of running it")]
    dry_run: bool,

    #[arg(long, global = true, help = "Never colour the output")]
    no_color: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

impl Cli {
    fn artifact_output(&self) -> bool {
        matches!(
            &self.command,
            Some(Commands::Build { output, extension, .. }) if *output || extension.is_some()
        )
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendChoice {
    /// AtmelStudio.exe, which is what the IDE itself runs.
    Studio,
    /// MSBuild, faster but unsupported by some Studio installations.
    Msbuild,
}

impl From<BackendChoice> for Backend {
    fn from(choice: BackendChoice) -> Self {
        match choice {
            BackendChoice::Studio => Self::Studio,
            BackendChoice::Msbuild => Self::MsBuild,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Build a configuration and report what the compiler said
    Build {
        #[arg(help = "Project name, from atp.toml or from the solution")]
        project: Option<String>,
        #[arg(short = 'c', long, help = "Build configuration, such as Debug")]
        configuration: Option<String>,
        #[arg(
            short = 'f',
            long,
            conflicts_with = "clean",
            help = "Build every file again"
        )]
        rebuild: bool,
        #[arg(
            short = 'o',
            long,
            conflicts_with = "clean",
            help = "Print the existing .elf path; add -f to rebuild first"
        )]
        output: bool,
        #[arg(
            short = 'e',
            long,
            conflicts_with = "clean",
            help = "Print an artifact path by extension; implies --output"
        )]
        extension: Option<String>,
        #[arg(long, help = "Delete the build output instead of building")]
        clean: bool,
        #[arg(
            last = true,
            help = "Extra arguments for the backend, after a -- separator"
        )]
        extra: Vec<String>,
    },
    /// Print the build artifacts directory, even if it does not exist
    Output {
        #[arg(help = "Project name, from atp.toml or from the solution")]
        project: Option<String>,
        #[arg(short = 'c', long, help = "Build configuration, such as Debug")]
        configuration: Option<String>,
    },
    /// List the projects atp can build
    Projects,
    /// List the configurations a project offers
    Configs {
        #[arg(help = "Project name, from atp.toml or from the solution")]
        project: Option<String>,
    },
}

pub(crate) fn run() -> AppResult<()> {
    execute(&Cli::parse())
}

fn execute(cli: &Cli) -> AppResult<()> {
    if cli.config_dir {
        let directory = config::user_directory().ok_or_else(|| {
            AppError::Runtime("cannot tell where your configuration directory is".into())
        })?;
        println!("{}", directory.display());
        return Ok(());
    }

    let command = cli
        .command
        .as_ref()
        .ok_or_else(|| AppError::Usage("a command is required. Run atp --help for help.".into()))?;

    let paths = Paths::detect()?;
    let config = Config::load(cli.config_file.as_deref())?;

    match command {
        Commands::Build {
            project,
            configuration,
            rebuild,
            output,
            extension,
            clean,
            extra,
        } => {
            let action = match (rebuild, clean) {
                (_, true) => Action::Clean,
                (true, _) => Action::Rebuild,
                _ => Action::Build,
            };
            let target = Target::resolve(
                cli,
                &config,
                &paths,
                project.as_deref(),
                configuration.as_deref(),
            )?;
            let lookup = *output || extension.is_some();
            if lookup && !rebuild && !extra.is_empty() {
                return Err(AppError::Usage(
                    "backend arguments require -f when looking up an artifact".into(),
                ));
            }
            if !lookup || *rebuild {
                command_build(cli, &paths, &config, &target, action, extra)?;
                if cli.dry_run {
                    return Ok(());
                }
            }
            if lookup {
                command_artifact(&target, extension.as_deref().unwrap_or("elf"))?;
            }
            Ok(())
        }
        Commands::Output {
            project,
            configuration,
        } => {
            let target = Target::resolve(
                cli,
                &config,
                &paths,
                project.as_deref(),
                configuration.as_deref(),
            )?;
            let directory = target.project.output_directory(&target.configuration);
            println!("{}", directory.display());
            Ok(())
        }
        Commands::Projects => command_projects(&config, &paths),
        Commands::Configs { project } => {
            let located = Located::find(cli, &config, &paths, project.as_deref())?;
            command_configs(&config, &located)
        }
    }
}

/// One project and configuration, resolved from every source that can name them.
struct Target {
    solution: Option<Solution>,
    project: Project,
    /// Passed to Studio when the solution holds more than one project.
    project_name: Option<String>,
    configuration: String,
}

/// A project, before a configuration has been chosen for it.
///
/// `atp configs` exists to show which configurations there are, so it has to
/// get this far without one.
struct Located {
    solution: Option<Solution>,
    project: Project,
    project_name: Option<String>,
    /// The name the user or atp.toml used, if any.
    name: Option<String>,
}

impl Located {
    fn find(cli: &Cli, config: &Config, paths: &Paths, wanted: Option<&str>) -> AppResult<Self> {
        let name = wanted.or(config.defaults.project.as_deref());
        let configured = name.and_then(|name| config.projects.get(name));

        // A name is either a solution named in atp.toml or a project inside the
        // solution atp finds for itself.
        let (path, inner_name) = match configured {
            Some(entry) => {
                let solution = entry.solution.as_deref().ok_or_else(|| {
                    AppError::Usage(format!(
                        "[projects.{}] in atp.toml has no solution path",
                        name.unwrap_or_default()
                    ))
                })?;
                let path = config.resolve(paths.to_native(solution)?);
                (path, entry.project.clone())
            }
            None => {
                let start = env::current_dir().map_err(|error| {
                    AppError::Runtime(format!("cannot read the working directory: {error}"))
                })?;
                let path = solution::discover(&start).map_err(|error| match name {
                    // The name matched nothing in atp.toml and there is no
                    // solution here either, so say both things at once.
                    Some(name) if !config.projects.is_empty() => AppError::Usage(format!(
                        "{name} is not in atp.toml, which names: {}\n{error}",
                        config.names().join(", ")
                    )),
                    _ => error,
                })?;
                (path, name.map(str::to_string))
            }
        };

        if !path.exists() {
            return Err(AppError::Usage(format!(
                "{} does not exist",
                path.display()
            )));
        }

        let path = std::path::absolute(&path).map_err(|error| {
            AppError::Runtime(format!("cannot resolve {}: {error}", path.display()))
        })?;
        let (solution, project, project_name) = open(&path, inner_name.as_deref())?;

        // MSBuild can build a bare project file, Studio cannot.
        if solution.is_none() && backend_for(cli, config) == Backend::Studio {
            return Err(AppError::Usage(format!(
                "{} has no solution beside it, and Atmel Studio cannot build a project on its own.\n\
                 Create a solution for it, or build with --backend msbuild.",
                project.path.display()
            )));
        }

        Ok(Self {
            solution,
            project,
            project_name,
            name: name.map(str::to_string),
        })
    }

    /// The configuration names this project offers, in the solution's order.
    fn configurations(&self) -> Vec<String> {
        configurations(self.solution.as_ref(), &self.project)
    }
}

impl Target {
    fn resolve(
        cli: &Cli,
        config: &Config,
        paths: &Paths,
        wanted: Option<&str>,
        configuration: Option<&str>,
    ) -> AppResult<Self> {
        let located = Located::find(cli, config, paths, wanted)?;
        let configuration = choose_configuration(configuration, config, &located)?;
        Ok(Self {
            solution: located.solution,
            project: located.project,
            project_name: located.project_name,
            configuration,
        })
    }

    fn label(&self) -> String {
        format!("{} [{}]", self.project.name, self.configuration)
    }
}

/// Opens a solution or a project file, and finds whichever one was not named.
fn open(
    path: &Path,
    wanted: Option<&str>,
) -> AppResult<(Option<Solution>, Project, Option<String>)> {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if extension == "atsln" {
        let solution = Solution::load(path)?;
        let entry = solution.entry(wanted)?.clone();
        let project = Project::load(&solution.project_path(&entry))?;
        // Studio only needs telling which project when there is a choice.
        let name = (solution.entries.len() > 1).then(|| entry.name.clone());
        return Ok((Some(solution), project, name));
    }

    let project = Project::load(path)?;
    match solution::solution_for(path) {
        Some(found) => {
            let solution = Solution::load(&found)?;
            let name = (solution.entries.len() > 1).then(|| project.name.clone());
            Ok((Some(solution), project, name))
        }
        None => Ok((None, project, None)),
    }
}

/// Settles on a configuration name, or lists the ones that exist.
fn choose_configuration(
    explicit: Option<&str>,
    config: &Config,
    located: &Located,
) -> AppResult<String> {
    let available = located.configurations();
    let project = &located.project;

    let wanted = explicit
        .map(str::to_string)
        .or_else(|| configured_configuration(config, located.name.as_deref()))
        .or_else(|| config.defaults.configuration.clone());

    match wanted {
        Some(wanted) => available
            .iter()
            .find(|known| known.eq_ignore_ascii_case(&wanted))
            .cloned()
            .ok_or_else(|| {
                AppError::Usage(format!(
                    "{} has no configuration named {wanted}.\nIt offers: {}",
                    project.name,
                    available.join(", ")
                ))
            }),
        None if available.len() == 1 => Ok(available[0].clone()),
        None => Err(AppError::Usage(format!(
            "{} offers several configurations, so choose one with --configuration:\n  {}\n\
             Set a default with [defaults] configuration in atp.toml.",
            project.name,
            available.join("\n  ")
        ))),
    }
}

/// The configuration a named project asked for in atp.toml.
fn configured_configuration(config: &Config, name: Option<&str>) -> Option<String> {
    config
        .projects
        .get(name?)
        .and_then(|entry| entry.configuration.clone())
}

/// The configuration names, preferring the solution's order.
fn configurations(solution: Option<&Solution>, project: &Project) -> Vec<String> {
    let from_solution = solution
        .map(|solution| solution.configurations.clone())
        .unwrap_or_default();
    if !from_solution.is_empty() {
        return from_solution;
    }
    project.configurations.clone()
}

fn backend_for(cli: &Cli, config: &Config) -> Backend {
    if let Some(choice) = cli.backend {
        return choice.into();
    }
    match config.defaults.backend.as_deref() {
        Some(name) if name.eq_ignore_ascii_case("msbuild") => Backend::MsBuild,
        _ => Backend::Studio,
    }
}

fn command_build(
    cli: &Cli,
    paths: &Paths,
    config: &Config,
    target: &Target,
    action: Action,
    extra: &[String],
) -> AppResult<()> {
    let artifact_output = cli.artifact_output();
    let print_line = |message: std::fmt::Arguments<'_>| {
        if artifact_output {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    };
    let style = style_for(cli);
    let installation = Installation::locate(
        paths,
        cli.studio.as_deref().or(config.studio.root.as_deref()),
    )?;

    let solution = target
        .solution
        .as_ref()
        .map(|solution| solution.path.clone())
        .unwrap_or_else(|| target.project.path.clone());

    let request = Request {
        solution: &solution,
        project: &target.project,
        project_name: target.project_name.as_deref(),
        configuration: &target.configuration,
        action,
        backend: backend_for(cli, config),
        extra,
    };

    if cli.dry_run {
        let argv = builder::command_line(&request, paths, &installation)?;
        print_line(format_args!("{}", quote(&argv)));
        return Ok(());
    }

    if !cli.quiet {
        print_line(format_args!(
            "{} {}",
            style.cyan(&label(action.verb())),
            target.label()
        ));
    }

    let outcome = builder::run(&request, paths, &installation, cli.quiet, artifact_output)?;
    let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if cli.verbose {
        let text = fs::read_to_string(&outcome.log).unwrap_or_default();
        if artifact_output {
            eprint!("{text}");
        } else {
            print!("{text}");
        }
    }

    for line in report::render(
        &outcome.report,
        paths,
        &style,
        &working_directory,
        cli.verbose,
    ) {
        print_line(format_args!("{line}"));
    }

    let seconds = outcome.elapsed.as_secs_f64();
    let counts = format!(
        "{}, {}",
        plural(outcome.report.errors(), "error"),
        plural(outcome.report.warnings(), "warning")
    );

    if !outcome.report.succeeded || outcome.exit_code != Some(0) {
        print_line(format_args!(
            "{} {} in {seconds:.1}s \u{2014} {counts}",
            style.red(&label("Failed")),
            target.label()
        ));
        print_line(format_args!(
            "{} {}",
            style.dim(&label("Log")),
            relative_to(&outcome.log, &working_directory)
        ));
        if let Some(code) = outcome.exit_code
            && code == 0
        {
            // Studio reports success in its exit code even when nothing built.
            print_line(format_args!(
                "{} {}",
                style.dim(&label("note")),
                style.dim("Atmel Studio exited 0; the log is what decided this")
            ));
        }
        return Err(AppError::Exit(1));
    }

    if cli.quiet {
        return Ok(());
    }

    let verb = if action == Action::Clean {
        "Cleaned"
    } else {
        "Finished"
    };
    print_line(format_args!(
        "{} {} in {seconds:.1}s \u{2014} {counts}",
        style.green(&label(verb)),
        target.label()
    ));

    if action == Action::Clean {
        return Ok(());
    }

    if let (Some(program), Some(data)) = (outcome.report.program, outcome.report.data) {
        print_line(format_args!(
            "{} Program {} B ({:.1}% full) \u{b7} Data {} B ({:.1}% full)",
            style.cyan(&label("Memory")),
            thousands(program.bytes),
            program.percent,
            thousands(data.bytes),
            data.percent
        ));
    }

    let artifact = target
        .project
        .artifact(&target.configuration, target.project.default_extension());
    if let Ok(metadata) = fs::metadata(&artifact) {
        print_line(format_args!(
            "{} {} ({})",
            style.cyan(&label("Output")),
            relative_to(&artifact, &working_directory),
            file_size(metadata.len())
        ));
    }

    Ok(())
}

fn command_artifact(target: &Target, extension: &str) -> AppResult<()> {
    let path = target.project.artifact(&target.configuration, extension);
    if !path.is_file() {
        return Err(missing(target, extension, &path));
    }
    warn_if_stale(target, std::slice::from_ref(&path));
    println!("{}", path.display());
    Ok(())
}

fn missing(target: &Target, extension: &str, path: &Path) -> AppError {
    let built = target.project.artifacts(&target.configuration);
    if built.is_empty() {
        return AppError::Runtime(format!(
            "{} has not been built yet. Run atp build first.",
            target.label()
        ));
    }
    let available: Vec<String> = built
        .iter()
        .filter_map(|path| path.extension())
        .map(|extension| extension.to_string_lossy().into_owned())
        .collect();
    AppError::Usage(format!(
        "{} produced no .{extension} file.\nIt produced: {}\n{}",
        target.label(),
        available.join(", "),
        path.display()
    ))
}

/// Says so when the artifacts are older than the sources they came from.
///
/// This goes to stderr so that it never lands in a pipeline that is only
/// after the path.
fn warn_if_stale(target: &Target, artifacts: &[PathBuf]) {
    let Some(oldest) = artifacts
        .iter()
        .filter_map(|path| fs::metadata(path).ok()?.modified().ok())
        .min()
    else {
        return;
    };
    let Some(newest) = newest_source(target) else {
        return;
    };
    if newest > oldest {
        eprintln!(
            "note: {} is older than the sources. Run atp build.",
            target.label()
        );
    }
}

/// The most recent change anywhere in the project, ignoring build output.
fn newest_source(target: &Target) -> Option<SystemTime> {
    let mut skip: Vec<String> = configurations(target.solution.as_ref(), &target.project);
    skip.extend([".git".into(), ".vs".into()]);
    let mut newest = None;
    walk(target.project.directory(), &skip, 12, &mut |modified| {
        if newest.is_none_or(|held| modified > held) {
            newest = Some(modified);
        }
    });
    newest
}

fn walk(directory: &Path, skip: &[String], depth: usize, found: &mut impl FnMut(SystemTime)) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            if depth == 0 {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !skip.iter().any(|skipped| *skipped == name) {
                walk(&path, skip, depth - 1, found);
            }
        } else if let Ok(modified) = metadata.modified() {
            found(modified);
        }
    }
}

fn command_projects(config: &Config, paths: &Paths) -> AppResult<()> {
    let mut printed = false;

    for (name, entry) in &config.projects {
        printed = true;
        let default = config.defaults.project.as_deref() == Some(name.as_str());
        let marker = if default { " (default)" } else { "" };
        println!("{name}{marker}");
        if let Some(description) = &entry.description {
            println!("    {description}");
        }
        if let Some(path) = &entry.solution {
            let shown = paths
                .to_native(path)
                .map(|path| config.resolve(path))
                .unwrap_or_else(|_| PathBuf::from(path));
            println!("    {}", shown.display());
        }
    }

    let here = env::current_dir()
        .ok()
        .and_then(|start| solution::discover(&start).ok());
    if let Some(path) = here
        && path.extension().is_some_and(|value| value == "atsln")
        && let Ok(solution) = Solution::load(&path)
    {
        if printed {
            println!();
        }
        println!("in {}:", path.display());
        for entry in &solution.entries {
            println!("    {}", entry.name);
        }
        printed = true;
    }

    if !printed {
        println!("no projects configured, and no solution found here");
    }
    Ok(())
}

fn command_configs(config: &Config, located: &Located) -> AppResult<()> {
    let names = located.configurations();
    if names.is_empty() {
        return Err(AppError::Runtime(format!(
            "{} declares no configurations",
            located.project.name
        )));
    }

    let project = &located.project;
    match &project.device {
        Some(device) => println!("{} ({device})", project.name),
        None => println!("{}", project.name),
    }

    let default = configured_configuration(config, located.name.as_deref())
        .or_else(|| config.defaults.configuration.clone());

    for configuration in names {
        let marker = match &default {
            Some(default) if default.eq_ignore_ascii_case(&configuration) => " (default)",
            _ => "",
        };
        println!("    {configuration}{marker}");
    }
    Ok(())
}

fn style_for(cli: &Cli) -> Style {
    let terminal = if cli.artifact_output() {
        io::stderr().is_terminal()
    } else {
        io::stdout().is_terminal()
    };
    let color = !cli.no_color && env::var_os("NO_COLOR").is_none() && terminal;
    Style::new(color)
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Renders a command the way a shell would need it typed.
fn quote(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if argument.contains(' ') {
                format!("\"{argument}\"")
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn accepts_the_project_name_on_either_side_of_the_option() {
        let first = Cli::parse_from(["atp", "build", "-e", "elf", "app"]);
        let second = Cli::parse_from(["atp", "build", "app", "-e", "elf"]);
        for cli in [first, second] {
            let Some(Commands::Build {
                project, extension, ..
            }) = cli.command
            else {
                panic!("expected the build command");
            };
            assert_eq!(project.as_deref(), Some("app"));
            assert_eq!(extension.as_deref(), Some("elf"));
        }
    }

    #[test]
    fn collects_backend_arguments_after_a_separator() {
        let cli = Cli::parse_from(["atp", "build", "--", "/p:Extra=1", "/m"]);
        let Some(Commands::Build { extra, .. }) = cli.command else {
            panic!("expected the build command");
        };
        assert_eq!(extra, vec!["/p:Extra=1", "/m"]);
    }

    #[test]
    fn refuses_to_clean_and_rebuild_at_once() {
        assert!(Cli::try_parse_from(["atp", "build", "--clean", "--rebuild"]).is_err());
    }

    #[test]
    fn counts_read_naturally() {
        assert_eq!(plural(0, "error"), "0 errors");
        assert_eq!(plural(1, "error"), "1 error");
        assert_eq!(plural(2, "warning"), "2 warnings");
    }

    #[test]
    fn quotes_only_the_arguments_that_need_it() {
        let argv = vec![
            "C:\\Program Files\\AtmelStudio.exe".to_string(),
            "/Build".to_string(),
            "Debug".to_string(),
        ];
        assert_eq!(
            quote(&argv),
            "\"C:\\Program Files\\AtmelStudio.exe\" /Build Debug"
        );
    }
}
