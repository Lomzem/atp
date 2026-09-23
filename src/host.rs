//! Where atp is running, and how to spell a path for the other side.
//!
//! atp drives Windows executables. On Windows that is the ordinary case. Under
//! WSL the same executables are reached through interop, so every path handed
//! to them has to be rewritten from `/mnt/c/...` to `C:\...`, and every path
//! printed back to the user has to be rewritten the other way.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{AppError, AppResult};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Host {
    /// A native Windows executable.
    Windows,
    /// A Linux executable inside WSL, driving Windows tools through interop.
    Wsl,
}

/// A drive letter mounted into the WSL file system, such as `C:` at `/mnt/c`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    point: String,
    drive: char,
}

/// The drive mappings that translate between Linux and Windows paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mounts {
    mounts: Vec<Mount>,
}

impl Mounts {
    /// The mappings WSL uses when nothing has been configured.
    fn automount() -> Self {
        let mounts = ('a'..='z')
            .map(|drive| Mount {
                point: format!("/mnt/{drive}"),
                drive,
            })
            .collect();
        Self { mounts }
    }

    /// Reads the live mappings, falling back to the usual `/mnt` layout.
    fn current() -> Self {
        match fs::read_to_string("/proc/mounts") {
            Ok(text) => {
                let mounts = Self::parse(&text);
                if mounts.mounts.is_empty() {
                    Self::automount()
                } else {
                    mounts
                }
            }
            Err(_) => Self::automount(),
        }
    }

    /// Reads drive mappings out of `/proc/mounts` text.
    ///
    /// Longer mount points come first so that the most specific one wins.
    fn parse(text: &str) -> Self {
        let mut mounts = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(source), Some(point), Some(kind)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if kind != "drvfs" && kind != "9p" && kind != "virtiofs" {
                continue;
            }
            let source = unescape(source);
            let mut characters = source.chars();
            let (Some(drive), Some(':')) = (characters.next(), characters.next()) else {
                continue;
            };
            if !drive.is_ascii_alphabetic() {
                continue;
            }
            mounts.push(Mount {
                point: unescape(point).trim_end_matches('/').to_string(),
                drive: drive.to_ascii_lowercase(),
            });
        }
        mounts.sort_by(|a, b| b.point.len().cmp(&a.point.len()));
        Self { mounts }
    }

    /// Rewrites an absolute Linux path as a Windows path.
    fn to_windows(&self, path: &str) -> AppResult<String> {
        for mount in &self.mounts {
            let rest = match path.strip_prefix(&mount.point) {
                Some("") => "",
                Some(rest) if rest.starts_with('/') => rest,
                _ => continue,
            };
            let drive = mount.drive.to_ascii_uppercase();
            let rest = rest.trim_start_matches('/').replace('/', "\\");
            return Ok(format!("{drive}:\\{rest}"));
        }
        Err(AppError::Runtime(format!(
            "{path} is inside the WSL file system, which Atmel Studio cannot open.\n\
             Move the project onto a Windows drive, such as /mnt/c, and try again."
        )))
    }

    /// Rewrites a Windows path as a Linux path.
    fn to_linux(&self, path: &str) -> AppResult<PathBuf> {
        if let Some(rest) = path.strip_prefix("\\\\") {
            let share = rest.split(['\\', '/']).next().unwrap_or(rest);
            return Err(AppError::Runtime(format!(
                "{path} is a network path (\\\\{share}), which atp cannot translate."
            )));
        }
        let mut characters = path.chars();
        let (Some(drive), Some(':'), Some('\\' | '/')) =
            (characters.next(), characters.next(), characters.next())
        else {
            return Err(AppError::Runtime(format!("{path} is not an absolute path")));
        };
        let drive = drive.to_ascii_lowercase();
        let rest = path[3..].replace('\\', "/");
        for mount in &self.mounts {
            if mount.drive == drive {
                let point = &mount.point;
                let rest = rest.trim_end_matches('/');
                return Ok(PathBuf::from(format!("{point}/{rest}")));
            }
        }
        Err(AppError::Runtime(format!(
            "drive {}: is not mounted in this WSL distribution",
            drive.to_ascii_uppercase()
        )))
    }
}

/// Expands the octal escapes that `/proc/mounts` uses for spaces and backslashes.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(index) = rest.find('\\') {
        out.push_str(&rest[..index]);
        let escape = &rest[index + 1..];
        match escape
            .get(..3)
            .and_then(|digits| u8::from_str_radix(digits, 8).ok())
        {
            Some(byte) => {
                out.push(byte as char);
                rest = &escape[3..];
            }
            None => {
                out.push('\\');
                rest = escape;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Path translation for the host atp is running on.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    host: Host,
    mounts: Mounts,
}

impl Paths {
    /// Detects the host, refusing to run anywhere Atmel Studio cannot be reached.
    pub(crate) fn detect() -> AppResult<Self> {
        if cfg!(windows) {
            return Ok(Self {
                host: Host::Windows,
                mounts: Mounts { mounts: Vec::new() },
            });
        }
        if !running_under_wsl() {
            return Err(AppError::Runtime(
                "atp drives Atmel Studio, so it needs Windows or WSL.".into(),
            ));
        }
        Ok(Self {
            host: Host::Wsl,
            mounts: Mounts::current(),
        })
    }

    /// Checks that this host can start a Windows program at all.
    ///
    /// WSL runs `.exe` files through a binfmt handler that some distributions
    /// lose when systemd takes over binfmt registration. Without it every
    /// Windows program fails with a bare "Exec format error", which says
    /// nothing about the real problem.
    pub(crate) fn require_windows_programs(&self) -> AppResult<()> {
        if self.host == Host::Windows || interop_registered() {
            return Ok(());
        }
        Err(AppError::Runtime(
            "this WSL distribution cannot start Windows programs, so atp cannot reach Atmel Studio.\n\
             The WSLInterop binfmt handler is not registered. To restore it:\n  \
             1. put [interop] enabled = true in /etc/wsl.conf\n  \
             2. run sudo systemctl restart systemd-binfmt\n  \
             3. if it still fails, run wsl --shutdown in Windows and start the distribution again\n\
             Reading commands such as atp output and atp configs work without it."
                .into(),
        ))
    }

    /// Turns a path written in either flavour into one this host can open.
    ///
    /// A configuration file is shared between Windows and WSL, so a path in
    /// `atp.toml` may be written either way whichever host reads it.
    pub(crate) fn to_native(&self, text: &str) -> AppResult<PathBuf> {
        match self.host {
            Host::Windows => {
                if looks_like_linux_path(text) {
                    return Ok(Mounts::automount()
                        .to_windows(text)
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| PathBuf::from(text)));
                }
                Ok(PathBuf::from(text))
            }
            Host::Wsl => {
                if looks_like_windows_path(text) {
                    return self.mounts.to_linux(text);
                }
                Ok(PathBuf::from(text))
            }
        }
    }

    /// Spells a path the way a Windows executable needs to receive it.
    pub(crate) fn to_windows(&self, path: &Path) -> AppResult<String> {
        let text = path.to_string_lossy();
        match self.host {
            Host::Windows => Ok(text.into_owned()),
            Host::Wsl => self.mounts.to_windows(&text),
        }
    }

    /// Spells a Windows path the way the user's shell would write it.
    ///
    /// Build logs are written by Windows tools, so the paths inside them are
    /// Windows paths even when the user reads the report in WSL.
    pub(crate) fn from_windows(&self, text: &str) -> PathBuf {
        match self.host {
            Host::Windows => PathBuf::from(text),
            Host::Wsl => self
                .mounts
                .to_linux(text)
                .unwrap_or_else(|_| PathBuf::from(text)),
        }
    }
}

/// Whether WSL's handler for Windows executables is registered and enabled.
fn interop_registered() -> bool {
    const HANDLERS: [&str; 2] = [
        "/proc/sys/fs/binfmt_misc/WSLInterop",
        "/proc/sys/fs/binfmt_misc/WSLInterop-late",
    ];
    HANDLERS
        .iter()
        .any(|path| fs::read_to_string(path).is_ok_and(|status| enabled_in_binfmt_status(&status)))
}

/// A binfmt entry opens with either `enabled` or `disabled` on its own line.
fn enabled_in_binfmt_status(status: &str) -> bool {
    status
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "enabled")
}

fn running_under_wsl() -> bool {
    if env::var_os("WSL_DISTRO_NAME").is_some() || env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn looks_like_windows_path(text: &str) -> bool {
    let mut characters = text.chars();
    matches!(
        (characters.next(), characters.next(), characters.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    ) || text.starts_with("\\\\")
}

fn looks_like_linux_path(text: &str) -> bool {
    text.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_MOUNTS: &str = concat!(
        "rootfs / wslfs rw,noatime 0 0\n",
        "none /dev devtmpfs rw,nosuid 0 0\n",
        "C:\\134 /mnt/c drvfs rw,noatime,uid=1000 0 0\n",
        "D:\\134 /mnt/data\\040disk drvfs rw,noatime 0 0\n",
        "snapfuse /snap/core squashfs ro 0 0\n",
    );

    fn mounts() -> Mounts {
        Mounts::parse(PROC_MOUNTS)
    }

    #[test]
    fn reads_drive_mappings_from_proc_mounts() {
        assert_eq!(
            mounts().mounts,
            vec![
                Mount {
                    point: "/mnt/data disk".into(),
                    drive: 'd',
                },
                Mount {
                    point: "/mnt/c".into(),
                    drive: 'c',
                },
            ]
        );
    }

    #[test]
    fn ignores_mounts_that_are_not_windows_drives() {
        assert!(Mounts::parse("rootfs / wslfs rw 0 0\n").mounts.is_empty());
    }

    #[test]
    fn rewrites_linux_paths_as_windows_paths() {
        let mounts = mounts();
        assert_eq!(
            mounts.to_windows("/mnt/c/Users/dev/app.atsln").unwrap(),
            "C:\\Users\\dev\\app.atsln"
        );
        assert_eq!(mounts.to_windows("/mnt/c").unwrap(), "C:\\");
        assert_eq!(mounts.to_windows("/mnt/data disk/app").unwrap(), "D:\\app");
    }

    #[test]
    fn refuses_paths_outside_a_windows_drive() {
        let error = mounts().to_windows("/home/dev/app.atsln").unwrap_err();
        assert!(format!("{error}").contains("WSL file system"));
    }

    #[test]
    fn does_not_confuse_a_mount_point_with_a_longer_name() {
        // /mnt/cache must not be mistaken for the /mnt/c mount.
        assert!(mounts().to_windows("/mnt/cache/app").is_err());
    }

    #[test]
    fn rewrites_windows_paths_as_linux_paths() {
        let mounts = mounts();
        assert_eq!(
            mounts.to_linux("C:\\Users\\dev\\app.elf").unwrap(),
            PathBuf::from("/mnt/c/Users/dev/app.elf")
        );
        assert_eq!(
            mounts.to_linux("c:/Users/dev").unwrap(),
            PathBuf::from("/mnt/c/Users/dev")
        );
    }

    #[test]
    fn reports_unmounted_drives_and_network_paths() {
        let mounts = mounts();
        let error = mounts.to_linux("Z:\\share\\app").unwrap_err();
        assert!(format!("{error}").contains("not mounted"));
        let error = mounts
            .to_linux("\\\\wsl.localhost\\arch\\home")
            .unwrap_err();
        assert!(format!("{error}").contains("network path"));
    }

    #[test]
    fn reads_a_binfmt_status_without_confusing_disabled_for_enabled() {
        assert!(enabled_in_binfmt_status(
            "enabled\ninterpreter /init\nflags: PF\n"
        ));
        assert!(!enabled_in_binfmt_status("disabled\ninterpreter /init\n"));
        assert!(!enabled_in_binfmt_status(""));
    }

    #[test]
    fn recognises_path_flavours() {
        assert!(looks_like_windows_path("C:\\app"));
        assert!(looks_like_windows_path("c:/app"));
        assert!(looks_like_windows_path("\\\\server\\share"));
        assert!(!looks_like_windows_path("/mnt/c/app"));
        assert!(looks_like_linux_path("/mnt/c/app"));
        assert!(!looks_like_linux_path("app\\main.c"));
    }
}
