//! Turning an Atmel Studio build log into something worth reading.
//!
//! Studio writes nothing to the console, so atp always builds with a log file
//! and reads the result back out of it. The log is verbose MSBuild output; the
//! parts worth showing are the compiler diagnostics, the memory summary, and
//! whether the build actually succeeded.

use std::fmt::Write as _;
use std::path::Path;

use crate::host::Paths;

/// Warnings shown before the rest are folded into a count.
const WARNING_LIMIT: usize = 25;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Severity {
    Error,
    Warning,
}

/// One compiler or build-system message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    pub(crate) severity: Severity,
    /// The file as the log spells it, which is always a Windows path.
    pub(crate) file: Option<String>,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) message: String,
    /// True for messages about the generated makefile or the link step rather
    /// than about the user's source.
    pub(crate) from_build_system: bool,
    /// How many times the same message appeared.
    pub(crate) count: usize,
}

/// A memory usage figure reported after a successful link.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Memory {
    pub(crate) bytes: u64,
    pub(crate) percent: f64,
}

/// Everything atp learned from one build.
#[derive(Clone, Debug, Default)]
pub(crate) struct Report {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) notes: Vec<String>,
    pub(crate) program: Option<Memory>,
    pub(crate) data: Option<Memory>,
    pub(crate) succeeded: bool,
    pub(crate) summary: Option<String>,
}

impl Report {
    pub(crate) fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub(crate) fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, severity: Severity) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == severity && !diagnostic.from_build_system)
            .map(|diagnostic| diagnostic.count)
            .sum()
    }
}

/// Reads a build log.
///
/// `output_directory` is the configuration's output directory as a Windows
/// path, used to tell messages about generated files apart from messages about
/// the user's own source.
pub(crate) fn parse(log: &str, output_directory: Option<&str>) -> Report {
    let mut report = Report::default();
    // Only Atmel Studio writes the "========== Build:" tally; a bare MSBuild
    // run reports success with nothing but the "Build succeeded." line.
    let mut project_counts: Option<(usize, usize)> = None;
    let mut saw_succeeded_line = false;

    for line in log.lines() {
        let trimmed = line.trim();

        if trimmed == "Build succeeded." {
            saw_succeeded_line = true;
            continue;
        }
        if trimmed == "Build FAILED." {
            saw_succeeded_line = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("========== Build:") {
            report.summary = Some(trimmed.trim_matches('=').trim().to_string());
            project_counts = Some(parse_counts(rest));
            continue;
        }
        if let Some(memory) = parse_memory(trimmed, "Program Memory Usage") {
            report.program = Some(memory);
            continue;
        }
        if let Some(memory) = parse_memory(trimmed, "Data Memory Usage") {
            report.data = Some(memory);
            continue;
        }
        if is_note(trimmed) {
            push_unique(&mut report.notes, trimmed.to_string());
            continue;
        }

        // A diagnostic always starts at column zero; the source excerpt and
        // caret that follow it are indented, so anything indented is context.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if let Some(diagnostic) = parse_diagnostic(line, output_directory) {
            merge(&mut report.diagnostics, diagnostic);
        }
    }

    let real_errors = report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error);
    let projects_built = match project_counts {
        Some((succeeded, failed)) => failed == 0 && succeeded > 0,
        None => true,
    };
    report.succeeded = saw_succeeded_line && projects_built && !real_errors;
    report
}

/// "1 succeeded or up-to-date, 0 failed, 0 skipped"
fn parse_counts(text: &str) -> (usize, usize) {
    let mut succeeded = 0;
    let mut failed = 0;
    for part in text.split(',') {
        let part = part.trim().trim_end_matches('=').trim();
        let Some((number, label)) = part.split_once(' ') else {
            continue;
        };
        let Ok(value) = number.trim().parse::<usize>() else {
            continue;
        };
        if label.starts_with("succeeded") {
            succeeded = value;
        } else if label.starts_with("failed") {
            failed = value;
        }
    }
    (succeeded, failed)
}

/// "Program Memory Usage : 135656 bytes   25.9 % Full"
fn parse_memory(line: &str, label: &str) -> Option<Memory> {
    let rest = line.split_once(label)?.1;
    let rest = rest.trim_start_matches([' ', '\t', ':']);
    let mut words = rest.split_whitespace();
    let bytes = words.next()?.parse().ok()?;
    if words.next()? != "bytes" {
        return None;
    }
    let percent = words.next()?.parse().unwrap_or(0.0);
    Some(Memory { bytes, percent })
}

/// Messages the toolchain prints without a file, which still matter.
fn is_note(line: &str) -> bool {
    line.starts_with("cc1.exe:")
        || line.starts_with("cc1plus.exe:")
        || (line.starts_with("make:") && line.contains("Error"))
        || line.starts_with("collect2.exe:")
}

/// Reads one diagnostic line.
///
/// Atmel's build task rewrites compiler output into the MSBuild canonical form
/// `C:\path\file.c(line,column): severity: message`, with a second form for
/// project-level failures: `C:\path\file.cproj : error : message`.
fn parse_diagnostic(line: &str, output_directory: Option<&str>) -> Option<Diagnostic> {
    let (file, line_number, column, rest) = match split_positioned(line) {
        Some(parts) => parts,
        None => split_unpositioned(line)?,
    };

    let (severity, message) = split_severity(rest)?;
    let file_text: Option<String> = file.map(str::to_string);
    let from_build_system = file_text
        .as_deref()
        .is_some_and(|path| is_generated(path, output_directory));

    Some(Diagnostic {
        severity,
        file: file_text,
        line: line_number,
        column,
        message,
        from_build_system,
        count: 1,
    })
}

/// Splits `C:\path\file.c(295,6): rest`.
fn split_positioned(line: &str) -> Option<(Option<&str>, u32, u32, &str)> {
    let close = line.find("): ")?;
    let head = &line[..close];
    let open = head.rfind('(')?;
    let (line_number, column) = head[open + 1..].split_once(',')?;
    let line_number = line_number.trim().parse().ok()?;
    let column = column.trim().parse().ok()?;
    let file = &head[..open];
    if file.is_empty() {
        return None;
    }
    Some((Some(file), line_number, column, &line[close + 3..]))
}

/// Splits `C:\path\app.cproj : error : rest`.
fn split_unpositioned(line: &str) -> Option<(Option<&str>, u32, u32, &str)> {
    let marker = line.find(" : error").or_else(|| line.find(" : warning"))?;
    let file = line[..marker].trim();
    if file.is_empty() {
        return None;
    }
    Some((Some(file), 0, 0, line[marker + 3..].trim_start()))
}

/// Splits `warning: message`, `error MSB6003: message`, or `error  : message`.
fn split_severity(rest: &str) -> Option<(Severity, String)> {
    let (head, message) = rest.split_once(':')?;
    let head = head.trim();
    let word = head.split_whitespace().next().unwrap_or(head);
    let severity = match word {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let code = head[word.len()..].trim();
    let message = message.trim();
    if code.is_empty() {
        Some((severity, message.to_string()))
    } else {
        // Keep the MSBuild code, it is the searchable part of the message.
        Some((severity, format!("{code}: {message}")))
    }
}

/// Messages about generated files are build-system noise, not source problems.
///
/// The generated makefile reports `recipe for target 'src/main.o' failed` for
/// every compile error, and the link step warns about its own command line.
fn is_generated(file: &str, output_directory: Option<&str>) -> bool {
    let lowered = file.to_ascii_lowercase();
    if lowered.ends_with("makefile") || lowered.ends_with(".mk") {
        return true;
    }
    match output_directory {
        Some(directory) => lowered.starts_with(&directory.to_ascii_lowercase()),
        None => false,
    }
}

fn merge(diagnostics: &mut Vec<Diagnostic>, diagnostic: Diagnostic) {
    if let Some(existing) = diagnostics.iter_mut().find(|held| {
        held.severity == diagnostic.severity
            && held.file == diagnostic.file
            && held.line == diagnostic.line
            && held.column == diagnostic.column
            && held.message == diagnostic.message
    }) {
        existing.count += 1;
        return;
    }
    diagnostics.push(diagnostic);
}

fn push_unique(notes: &mut Vec<String>, note: String) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}

/// ANSI styling, switched off when the output is not a terminal.
#[derive(Clone, Copy)]
pub(crate) struct Style {
    color: bool,
}

impl Style {
    pub(crate) fn new(color: bool) -> Self {
        Self { color }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub(crate) fn green(&self, text: &str) -> String {
        self.paint("1;32", text)
    }

    pub(crate) fn red(&self, text: &str) -> String {
        self.paint("1;31", text)
    }

    pub(crate) fn yellow(&self, text: &str) -> String {
        self.paint("1;33", text)
    }

    pub(crate) fn cyan(&self, text: &str) -> String {
        self.paint("1;36", text)
    }

    pub(crate) fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
}

/// Right-aligns a label the way cargo does, so the messages line up.
pub(crate) fn label(text: &str) -> String {
    format!("{text:>10}")
}

/// Renders the report as the lines atp prints after a build.
pub(crate) fn render(
    report: &Report,
    paths: &Paths,
    style: &Style,
    working_directory: &Path,
    verbose: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut shown_warnings = 0usize;
    let mut hidden_warnings = 0usize;

    for diagnostic in &report.diagnostics {
        if diagnostic.from_build_system && !verbose {
            continue;
        }
        if diagnostic.severity == Severity::Warning && !verbose {
            if shown_warnings >= WARNING_LIMIT {
                hidden_warnings += diagnostic.count;
                continue;
            }
            shown_warnings += 1;
        }
        lines.push(render_diagnostic(
            diagnostic,
            paths,
            style,
            working_directory,
        ));
    }

    if hidden_warnings > 0 {
        lines.push(format!(
            "{} {}",
            style.dim(&label("...")),
            style.dim(&format!(
                "{hidden_warnings} more warnings, run with --verbose to see them"
            ))
        ));
    }

    for note in &report.notes {
        lines.push(format!("{} {}", style.dim(&label("note")), note));
    }

    lines
}

fn render_diagnostic(
    diagnostic: &Diagnostic,
    paths: &Paths,
    style: &Style,
    working_directory: &Path,
) -> String {
    let painted = match diagnostic.severity {
        Severity::Error => style.red(&label("error")),
        Severity::Warning => style.yellow(&label("warning")),
    };

    let mut text = String::new();
    if let Some(file) = &diagnostic.file {
        let native = paths.from_windows(file);
        let shown = relative_to(&native, working_directory);
        let _ = write!(text, "{shown}");
        if diagnostic.line > 0 {
            let _ = write!(text, ":{}:{}", diagnostic.line, diagnostic.column);
        }
        let _ = write!(text, ": ");
    }
    let _ = write!(text, "{}", diagnostic.message);
    if diagnostic.count > 1 {
        let _ = write!(text, " ({}x)", diagnostic.count);
    }
    format!("{painted} {text}")
}

/// Shortens a path to a relative one when that stays unambiguous.
pub(crate) fn relative_to(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Formats a byte count with thousands separators.
pub(crate) fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// Formats a file size the way a person reads it.
pub(crate) fn file_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 20, "MB"), (1 << 10, "KB"), (1, "B")];
    for (scale, unit) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes} {unit}");
            }
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: &str = concat!(
        "------ Build started: Project: app, Configuration: Debug ARM ------\n",
        "Build started.\n",
        "\t\tBuilding file: ../src/main.c\n",
        "C:\\work\\app\\src\\main.c(295,6): warning: unused variable 'unused' [-Wunused-variable]\n",
        "\t\t  int unused = 0;\n",
        "\t\t      ^~~~~~\n",
        "C:\\work\\app\\src\\main.c(301,9): error: implicit declaration of function 'missing' [-Werror=implicit-function-declaration]\n",
        "\t\t  return missing();\n",
        "\t\tcc1.exe: some warnings being treated as errors\n",
        "\t\tmake: *** [src/main.o] Error 1\n",
        "C:\\work\\app\\Debug\\Makefile(1946,1): error: recipe for target 'src/main.o' failed\n",
        "Build FAILED.\n",
        "========== Build: 0 succeeded or up-to-date, 1 failed, 0 skipped ==========\n",
    );

    const SUCCEEDED: &str = concat!(
        "------ Build started: Project: app, Configuration: Debug ARM ------\n",
        "C:\\work\\app\\Debug\\app.elf(0,0): warning: Command Line Exceeds Limit app.elf\n",
        "\t\t\tProgram Memory Usage \t:\t135656 bytes   25.9 % Full\n",
        "\t\t\tData Memory Usage \t\t:\t63048 bytes   48.1 % Full\n",
        "Build succeeded.\n",
        "========== Build: 1 succeeded or up-to-date, 0 failed, 0 skipped ==========\n",
    );

    #[test]
    fn reads_diagnostics_out_of_a_failed_build() {
        let report = parse(FAILED, Some("C:\\work\\app\\Debug"));
        assert!(!report.succeeded);
        assert_eq!(report.errors(), 1);
        assert_eq!(report.warnings(), 1);

        let error = report
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.severity == Severity::Error && !diagnostic.from_build_system
            })
            .unwrap();
        assert_eq!(error.file.as_deref(), Some("C:\\work\\app\\src\\main.c"));
        assert_eq!((error.line, error.column), (301, 9));
        assert!(error.message.starts_with("implicit declaration"));
    }

    #[test]
    fn treats_makefile_messages_as_build_system_noise() {
        let report = parse(FAILED, Some("C:\\work\\app\\Debug"));
        let makefile = report
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic
                    .file
                    .as_deref()
                    .is_some_and(|file| file.ends_with("Makefile"))
            })
            .unwrap();
        assert!(makefile.from_build_system);
        // It must not inflate the error count the user is shown.
        assert_eq!(report.errors(), 1);
    }

    #[test]
    fn keeps_the_toolchain_notes() {
        let report = parse(FAILED, None);
        assert!(report.notes.iter().any(|note| note.starts_with("cc1.exe:")));
        assert!(report.notes.iter().any(|note| note.contains("Error 1")));
    }

    #[test]
    fn ignores_indented_source_excerpts() {
        // "  int unused = 0;" must not be mistaken for a message.
        let report = parse(FAILED, None);
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("int unused"))
        );
    }

    #[test]
    fn reads_memory_usage_and_success() {
        let report = parse(SUCCEEDED, Some("C:\\work\\app\\Debug"));
        assert!(report.succeeded);
        assert_eq!(report.program.unwrap().bytes, 135_656);
        assert!((report.program.unwrap().percent - 25.9).abs() < 0.01);
        assert_eq!(report.data.unwrap().bytes, 63_048);
        // The linker's own warning is not the user's problem.
        assert_eq!(report.warnings(), 0);
    }

    #[test]
    fn does_not_call_a_build_successful_when_no_project_built() {
        // Atmel Studio exits zero when a project fails to load, so the counts
        // in the summary line are what decide.
        let log = concat!(
            "C:\\work\\app\\app.cproj : error  : Value cannot be null.\n",
            "========== Build: 0 succeeded or up-to-date, 0 failed, 0 skipped ==========\n",
        );
        let report = parse(log, None);
        assert!(!report.succeeded);
        assert_eq!(report.errors(), 1);
        assert_eq!(report.diagnostics[0].message, "Value cannot be null.");
    }

    #[test]
    fn keeps_msbuild_error_codes() {
        let log = "C:\\a\\b.targets(31,5): error MSB6003: The task could not be run.\n";
        let report = parse(log, None);
        assert_eq!(
            report.diagnostics[0].message,
            "MSB6003: The task could not be run."
        );
    }

    #[test]
    fn folds_repeated_messages_together() {
        let line = "C:\\work\\app\\src\\util.h(4,1): warning: unused parameter 'x' [-Wunused]\n";
        let report = parse(&line.repeat(3), None);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].count, 3);
        assert_eq!(report.warnings(), 3);
    }

    /// Logs captured from a real Atmel Studio 7 build, trimmed and stripped of
    /// anything identifying. They are the guard against the format drifting.
    const REAL_SUCCESS: &str = include_str!("../tests/fixtures/logs/success.log");
    const REAL_FAILURE: &str = include_str!("../tests/fixtures/logs/failure.log");

    #[test]
    fn reads_a_real_successful_build() {
        let report = parse(REAL_SUCCESS, Some("C:\\work\\app\\Debug"));
        assert!(report.succeeded);
        assert_eq!(report.errors(), 0);
        // The only warning is the linker task complaining about its own
        // command line, which is not the user's problem.
        assert_eq!(report.warnings(), 0);
        assert_eq!(report.program.unwrap().bytes, 135_656);
        assert_eq!(report.data.unwrap().bytes, 63_048);
        assert_eq!(
            report.summary.as_deref(),
            Some("Build: 1 succeeded or up-to-date, 0 failed, 0 skipped")
        );
    }

    #[test]
    fn reads_a_real_failed_build() {
        let report = parse(REAL_FAILURE, Some("C:\\work\\app\\Debug"));
        assert!(!report.succeeded);
        assert_eq!(report.errors(), 1);
        assert_eq!(report.warnings(), 4);

        let error = report
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.severity == Severity::Error && !diagnostic.from_build_system
            })
            .unwrap();
        assert_eq!((error.line, error.column), (301, 9));
        assert!(error.message.contains("implicit declaration"));
        assert!(report.notes.iter().any(|note| note.contains("cc1.exe")));
    }

    #[test]
    fn does_not_mistake_gcc_context_lines_for_messages() {
        // Lines such as "../src/main.c: In function 'helper':" and the caret
        // rows sit between the diagnostics in a real log.
        let report = parse(REAL_FAILURE, Some("C:\\work\\app\\Debug"));
        assert_eq!(report.diagnostics.len(), 6);
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("In function"))
        );
    }

    #[test]
    fn formats_numbers_for_reading() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(135_656), "135,656");
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(2048), "2.0 KB");
    }
}
