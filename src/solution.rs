//! Reading Atmel Studio solution and project files.
//!
//! An `.atsln` lists the projects and the configuration names. A `.cproj` is an
//! MSBuild file that says where the build output lands and what it is called.
//! Between them they describe everything atp needs, so a project directory
//! usually needs no configuration file at all.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{AppError, AppResult};

/// Artifact extensions in the order a person is most likely to want them.
const KNOWN_EXTENSIONS: [&str; 7] = ["elf", "bin", "hex", "srec", "eep", "lss", "map"];

/// How deep `atp` looks below the working directory for a solution.
const SEARCH_DEPTH: usize = 4;

/// Directories that never hold a solution worth finding.
const SKIPPED_DIRECTORIES: [&str; 6] = [".git", ".vs", "node_modules", "target", "obj", "bin"];

/// One project entry in a solution file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SolutionEntry {
    pub(crate) name: String,
    /// The path as written in the solution file, relative to it.
    pub(crate) relative_path: String,
}

/// An `.atsln` solution file.
#[derive(Clone, Debug)]
pub(crate) struct Solution {
    pub(crate) path: PathBuf,
    pub(crate) entries: Vec<SolutionEntry>,
    pub(crate) configurations: Vec<String>,
}

impl Solution {
    pub(crate) fn load(path: &Path) -> AppResult<Self> {
        let text = read(path)?;
        let mut solution = Self::parse(&text);
        solution.path = path.to_path_buf();
        Ok(solution)
    }

    fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        let mut configurations: Vec<String> = Vec::new();
        let mut in_configuration_section = false;

        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("GlobalSection(") {
                in_configuration_section = rest.starts_with("SolutionConfigurationPlatforms");
                continue;
            }
            if line == "EndGlobalSection" {
                in_configuration_section = false;
                continue;
            }
            if in_configuration_section {
                // Debug|ARM = Debug|ARM
                if let Some((left, _)) = line.split_once('=') {
                    let name = left.trim().split('|').next().unwrap_or_default().trim();
                    if !name.is_empty() && !configurations.iter().any(|held| held == name) {
                        configurations.push(name.to_string());
                    }
                }
                continue;
            }
            if line.starts_with("Project(") {
                if let Some(entry) = parse_project_line(line) {
                    entries.push(entry);
                }
            }
        }

        Self {
            path: PathBuf::new(),
            entries,
            configurations,
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Resolves a solution entry to the project file it points at.
    pub(crate) fn project_path(&self, entry: &SolutionEntry) -> PathBuf {
        self.directory().join(
            entry
                .relative_path
                .replace('\\', std::path::MAIN_SEPARATOR_STR),
        )
    }

    /// Picks the entry the user meant, or explains the choice they have to make.
    pub(crate) fn entry(&self, wanted: Option<&str>) -> AppResult<&SolutionEntry> {
        match wanted {
            Some(name) => self
                .entries
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    AppError::Usage(format!(
                        "{} has no project named {name}.\nIt holds: {}",
                        self.path.display(),
                        self.names().join(", ")
                    ))
                }),
            None if self.entries.len() == 1 => Ok(&self.entries[0]),
            None if self.entries.is_empty() => Err(AppError::Runtime(format!(
                "{} lists no projects",
                self.path.display()
            ))),
            None => Err(AppError::Usage(format!(
                "{} holds several projects, so name the one to build with --project.\nIt holds: {}",
                self.path.display(),
                self.names().join(", ")
            ))),
        }
    }

    fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }
}

/// Project("{TYPE-GUID}") = "name", "relative\path.cproj", "{PROJECT-GUID}"
fn parse_project_line(line: &str) -> Option<SolutionEntry> {
    let (_, rest) = line.split_once('=')?;
    let mut fields = rest.split(',').map(|field| field.trim().trim_matches('"'));
    let name = fields.next()?.to_string();
    let relative_path = fields.next()?.to_string();
    if name.is_empty() || !relative_path.to_ascii_lowercase().ends_with(".cproj") {
        return None;
    }
    Some(SolutionEntry {
        name,
        relative_path,
    })
}

/// A `.cproj` project file, reduced to the handful of properties atp uses.
#[derive(Clone, Debug)]
pub(crate) struct Project {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) device: Option<String>,
    /// Configuration names that carry their own settings in the project file.
    pub(crate) configurations: Vec<String>,
    output_directory: String,
    output_file_name: String,
    output_extension: String,
}

impl Project {
    pub(crate) fn load(path: &Path) -> AppResult<Self> {
        let text = read(path)?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .ok_or_else(|| AppError::Runtime(format!("{} has no file name", path.display())))?;
        Ok(Self::parse(&text, path, &name))
    }

    fn parse(xml: &str, path: &Path, name: &str) -> Self {
        let groups = property_groups(xml);
        let global: String = groups
            .iter()
            .filter(|group| group.condition.is_none())
            .map(|group| group.body)
            .collect::<Vec<_>>()
            .join("\n");

        let configurations = groups
            .iter()
            .filter_map(|group| group.condition.as_deref().and_then(configuration_of))
            .fold(Vec::new(), |mut held: Vec<String>, name| {
                if !held.iter().any(|seen| seen == name) {
                    held.push(name.to_string());
                }
                held
            });

        Self {
            path: path.to_path_buf(),
            name: name.to_string(),
            device: tag_value(&global, "avrdevice").map(str::to_string),
            configurations,
            output_directory: tag_value(&global, "OutputDirectory")
                .unwrap_or("$(MSBuildProjectDirectory)\\$(Configuration)")
                .to_string(),
            output_file_name: tag_value(&global, "OutputFileName")
                .unwrap_or("$(MSBuildProjectName)")
                .to_string(),
            output_extension: tag_value(&global, "OutputFileExtension")
                .unwrap_or(".elf")
                .trim_start_matches('.')
                .to_string(),
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Where a configuration writes its build output.
    pub(crate) fn output_directory(&self, configuration: &str) -> PathBuf {
        PathBuf::from(self.expand(&self.output_directory, configuration))
    }

    /// The base name shared by every artifact, without an extension.
    pub(crate) fn output_stem(&self, configuration: &str) -> String {
        self.expand(&self.output_file_name, configuration)
    }

    /// The extension of the linked image, normally `elf`.
    pub(crate) fn default_extension(&self) -> &str {
        &self.output_extension
    }

    /// The path of one artifact, whether or not it has been built yet.
    pub(crate) fn artifact(&self, configuration: &str, extension: &str) -> PathBuf {
        let extension = extension.trim_start_matches('.');
        let stem = self.output_stem(configuration);
        self.output_directory(configuration)
            .join(format!("{stem}.{extension}"))
    }

    /// Every artifact this configuration has actually produced.
    pub(crate) fn artifacts(&self, configuration: &str) -> Vec<PathBuf> {
        let directory = self.output_directory(configuration);
        let stem = self.output_stem(configuration);
        let Ok(entries) = fs::read_dir(&directory) else {
            return Vec::new();
        };

        let mut found: Vec<(usize, PathBuf)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_stem()
                    .is_some_and(|found| found.to_string_lossy() == stem)
            })
            .filter_map(|path| {
                let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
                let rank = KNOWN_EXTENSIONS
                    .iter()
                    .position(|known| *known == extension)?;
                Some((rank, path))
            })
            .collect();

        found.sort_by_key(|(rank, _)| *rank);
        found.into_iter().map(|(_, path)| path).collect()
    }

    /// Expands the MSBuild properties that appear in Atmel's project templates.
    fn expand(&self, template: &str, configuration: &str) -> String {
        let directory = self.directory().to_string_lossy().into_owned();
        let expanded = template
            .replace("$(MSBuildProjectDirectory)", &directory)
            .replace("$(MSBuildProjectName)", &self.name)
            .replace("$(AssemblyName)", &self.name)
            .replace("$(Configuration)", configuration);
        normalize_separators(&expanded)
    }
}

/// Rewrites separators the way the running host expects them.
fn normalize_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

struct PropertyGroup<'a> {
    condition: Option<String>,
    body: &'a str,
}

/// Splits a project file into its `<PropertyGroup>` blocks.
///
/// Atmel projects never nest property groups, so matching the next closing tag
/// is enough and avoids pulling in an XML parser for six values.
fn property_groups(xml: &str) -> Vec<PropertyGroup<'_>> {
    let mut groups = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<PropertyGroup") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let open_tag = &after[..open_end];
        let body_start = open_end + 1;
        let Some(close) = after.find("</PropertyGroup>") else {
            break;
        };
        if close > body_start {
            groups.push(PropertyGroup {
                condition: attribute(open_tag, "Condition"),
                body: &after[body_start..close],
            });
        }
        rest = &after[close + "</PropertyGroup>".len()..];
    }
    groups
}

/// Reads an attribute out of an opening tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Reads the configuration name out of `' $(Configuration)' == 'Debug' `.
fn configuration_of(condition: &str) -> Option<&str> {
    let (left, right) = condition.split_once("==")?;
    if !left.contains("$(Configuration)") {
        return None;
    }
    let name = right.trim().trim_matches('\'').trim();
    if name.is_empty() { None } else { Some(name) }
}

/// Reads the text of the first `<tag>value</tag>` element.
fn tag_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find(&close)?;
    let value = rest[..end].trim();
    if value.is_empty() { None } else { Some(value) }
}

fn read(path: &Path) -> AppResult<String> {
    let bytes = fs::read(path)
        .map_err(|error| AppError::Runtime(format!("cannot read {}: {error}", path.display())))?;
    // Atmel writes these files with a UTF-8 byte order mark.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

/// Looks for the solution or project the user most likely means.
///
/// Walking up finds the solution from anywhere inside a project tree, which is
/// where builds are usually started. Only when that fails does atp search
/// downwards, so running it one directory above a checkout still works.
pub(crate) fn discover(start: &Path) -> AppResult<PathBuf> {
    for directory in start.ancestors() {
        if let Some(found) = single_match(directory, "atsln")? {
            return Ok(found);
        }
    }
    if let Some(found) = search_downwards(start, "atsln", SEARCH_DEPTH)? {
        return Ok(found);
    }
    if let Some(found) = search_downwards(start, "cproj", SEARCH_DEPTH)? {
        return Ok(found);
    }
    Err(AppError::Usage(format!(
        "no .atsln or .cproj found in or below {}.\n\
         Run atp from a project directory, or name a solution in atp.toml.",
        start.display()
    )))
}

/// Finds files with an extension in one directory, refusing an ambiguous answer.
fn single_match(directory: &Path, extension: &str) -> AppResult<Option<PathBuf>> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(None);
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| has_extension(path, extension))
        .collect();
    found.sort();
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0))),
        _ => Err(AppError::Usage(format!(
            "{} holds several .{extension} files, so name the one to use:\n  {}",
            directory.display(),
            found
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        ))),
    }
}

fn search_downwards(start: &Path, extension: &str, depth: usize) -> AppResult<Option<PathBuf>> {
    let mut found = Vec::new();
    collect(start, extension, depth, &mut found);
    found.sort();
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0))),
        _ => Err(AppError::Usage(format!(
            "several .{extension} files sit below {}, so name the one to use:\n  {}",
            start.display(),
            found
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        ))),
    }
}

fn collect(directory: &Path, extension: &str, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut subdirectories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirectories.push(path);
        } else if has_extension(&path, extension) {
            found.push(path);
        }
    }
    if depth == 0 {
        return;
    }
    for subdirectory in subdirectories {
        let skip = subdirectory
            .file_name()
            .map(|name| {
                let name = name.to_string_lossy();
                SKIPPED_DIRECTORIES.contains(&name.as_ref())
            })
            .unwrap_or(false);
        if !skip {
            collect(&subdirectory, extension, depth - 1, found);
        }
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|found| found.to_string_lossy().eq_ignore_ascii_case(extension))
}

/// Finds a solution that builds the given project file.
///
/// Atmel Studio refuses to build a bare `.cproj`: without a solution it fails
/// with `Value cannot be null. Parameter name: url`. Given a project file, atp
/// looks for the solution that owns it rather than passing the project through.
pub(crate) fn solution_for(project: &Path) -> Option<PathBuf> {
    let directory = project.parent()?;
    let candidates = [directory, directory.parent()?];
    let wanted = project.canonicalize().ok()?;

    for candidate in candidates {
        let Ok(entries) = fs::read_dir(candidate) else {
            continue;
        };
        let mut solutions: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| has_extension(path, "atsln"))
            .collect();
        solutions.sort();
        for path in solutions {
            let Ok(solution) = Solution::load(&path) else {
                continue;
            };
            let owns = solution.entries.iter().any(|entry| {
                solution
                    .project_path(entry)
                    .canonicalize()
                    .is_ok_and(|resolved| resolved == wanted)
            });
            if owns {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOLUTION: &str = concat!(
        "\nMicrosoft Visual Studio Solution File, Format Version 12.00\n",
        "# Atmel Studio Solution File, Format Version 11.00\n",
        "Project(\"{54F91283-7BC4-4236-8FF9-10F437C3AD48}\") = \"app\", ",
        "\"app\\app.cproj\", \"{DCE6C7E3-EE26-4D79-826B-08594B9AD897}\"\n",
        "EndProject\n",
        "Global\n",
        "\tGlobalSection(SolutionConfigurationPlatforms) = preSolution\n",
        "\t\tDebug|ARM = Debug|ARM\n",
        "\t\tRelease_to_application_partition|ARM = Release_to_application_partition|ARM\n",
        "\t\tRelease|ARM = Release|ARM\n",
        "\tEndGlobalSection\n",
        "\tGlobalSection(ProjectConfigurationPlatforms) = postSolution\n",
        "\t\t{DCE6C7E3-EE26-4D79-826B-08594B9AD897}.Debug|ARM.ActiveCfg = Debug|ARM\n",
        "\tEndGlobalSection\n",
        "EndGlobal\n",
    );

    const PROJECT: &str = concat!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
        "<Project DefaultTargets=\"Build\" xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">\n",
        "  <PropertyGroup>\n",
        "    <ToolchainName>com.Atmel.ARMGCC.C</ToolchainName>\n",
        "    <avrdevice>ATSAM4E8C</avrdevice>\n",
        "    <OutputType>Executable</OutputType>\n",
        "    <OutputFileName>$(MSBuildProjectName)</OutputFileName>\n",
        "    <OutputFileExtension>.elf</OutputFileExtension>\n",
        "    <OutputDirectory>$(MSBuildProjectDirectory)\\$(Configuration)</OutputDirectory>\n",
        "    <UncachedRange />\n",
        "  </PropertyGroup>\n",
        "  <PropertyGroup Condition=\" '$(Configuration)' == 'Release' \">\n",
        "    <ToolchainSettings><ArmGcc>\n",
        "      <armgcc.common.outputfiles.hex>True</armgcc.common.outputfiles.hex>\n",
        "    </ArmGcc></ToolchainSettings>\n",
        "  </PropertyGroup>\n",
        "  <PropertyGroup Condition=\" '$(Configuration)' == 'Debug' \">\n",
        "    <ToolchainSettings><ArmGcc /></ToolchainSettings>\n",
        "  </PropertyGroup>\n",
        "</Project>\n",
    );

    fn project() -> Project {
        Project::parse(PROJECT, Path::new("/work/app/app.cproj"), "app")
    }

    #[test]
    fn reads_projects_and_configurations_from_a_solution() {
        let solution = Solution::parse(SOLUTION);
        assert_eq!(
            solution.entries,
            vec![SolutionEntry {
                name: "app".into(),
                relative_path: "app\\app.cproj".into(),
            }]
        );
        assert_eq!(
            solution.configurations,
            vec!["Debug", "Release_to_application_partition", "Release"]
        );
    }

    #[test]
    fn ignores_the_project_configuration_section() {
        // Those lines also contain '=' and would otherwise add bogus names.
        let solution = Solution::parse(SOLUTION);
        assert!(
            !solution
                .configurations
                .iter()
                .any(|name| name.contains('{'))
        );
    }

    #[test]
    fn picks_the_only_project_without_being_asked() {
        let solution = Solution::parse(SOLUTION);
        assert_eq!(solution.entry(None).unwrap().name, "app");
        assert_eq!(solution.entry(Some("APP")).unwrap().name, "app");
    }

    #[test]
    fn explains_an_unknown_project_name() {
        let solution = Solution::parse(SOLUTION);
        let error = solution.entry(Some("other")).unwrap_err();
        assert!(format!("{error}").contains("no project named other"));
    }

    #[test]
    fn reads_the_properties_atp_needs() {
        let project = project();
        assert_eq!(project.device.as_deref(), Some("ATSAM4E8C"));
        assert_eq!(project.default_extension(), "elf");
        assert_eq!(project.configurations, vec!["Release", "Debug"]);
    }

    #[test]
    fn expands_msbuild_properties_in_the_output_path() {
        let project = project();
        let expected = Path::new("/work/app").join("Debug").join("app.elf");
        assert_eq!(project.artifact("Debug", "elf"), expected);
        assert_eq!(project.artifact("Debug", ".elf"), expected);
        assert_eq!(project.output_stem("Debug"), "app");
    }

    #[test]
    fn keeps_configuration_names_with_underscores_intact() {
        let project = project();
        let directory = project.output_directory("Release_to_application_partition");
        assert!(directory.ends_with("Release_to_application_partition"));
    }

    #[test]
    fn reads_a_condition_attribute() {
        assert_eq!(
            configuration_of(" '$(Configuration)' == 'Debug' "),
            Some("Debug")
        );
        assert_eq!(configuration_of(" '$(Platform)' == 'ARM' "), None);
    }

    #[test]
    fn treats_a_self_closing_tag_as_absent() {
        assert_eq!(tag_value("<a><UncachedRange /></a>", "UncachedRange"), None);
    }
}
