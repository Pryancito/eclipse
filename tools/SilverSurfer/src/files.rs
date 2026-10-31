//! The file dialog is lunarfile. SilverSurfer does not draw one of its own.
//!
//! `--pick` prints nothing and exits non-zero when the user cancels. The
//! chosen paths, one per line, land in the file passed to `--out`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What the page asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickRequest {
    pub multiple: bool,
    /// `Some` turns the dialog into lunarfile's save field.
    pub save_name: Option<String>,
}

/// The words after the program name.
pub fn arguments(request: &PickRequest, out: &Path) -> Vec<String> {
    let mut args = vec!["--pick".to_string(), "--out".to_string(), path_arg(out)];
    if request.multiple {
        args.push("--multiple".to_string());
    }
    if let Some(name) = &request.save_name {
        args.push("--save".to_string());
        args.push(name.clone());
    }
    args
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run lunarfile and return the chosen paths.
///
/// An empty `Ok` is a cancellation (Escape, or a non-zero exit). `Err` means
/// the dialog could not be started.
pub fn pick(request: &PickRequest) -> Result<Vec<PathBuf>, String> {
    let program = find_lunarfile().ok_or_else(|| {
        "lunarfile no está en PATH ni en /usr/local/bin".to_string()
    })?;
    let out = std::env::temp_dir().join(format!("silversurfer-pick-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(&program)
        .args(arguments(request, &out))
        .status()
        .map_err(|e| format!("no se pudo ejecutar {}: {e}", program.display()))?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn find_lunarfile() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("lunarfile");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for dir in ["/usr/local/bin", "/usr/bin", "/bin"] {
        let candidate = Path::new(dir).join("lunarfile");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_one_file() {
        let args = arguments(
            &PickRequest {
                multiple: false,
                save_name: None,
            },
            Path::new("/tmp/out"),
        );
        assert_eq!(args, vec!["--pick", "--out", "/tmp/out"]);
    }

    #[test]
    fn several_files_and_save() {
        let args = arguments(
            &PickRequest {
                multiple: true,
                save_name: Some("notas.txt".into()),
            },
            Path::new("/tmp/out"),
        );
        assert_eq!(
            args,
            vec!["--pick", "--out", "/tmp/out", "--multiple", "--save", "notas.txt"]
        );
    }

    #[test]
    fn a_save_without_multiple_names_only_the_file() {
        let args = arguments(
            &PickRequest {
                multiple: false,
                save_name: Some("informe.html".into()),
            },
            Path::new("/tmp/elegidos"),
        );
        assert_eq!(
            args,
            vec!["--pick", "--out", "/tmp/elegidos", "--save", "informe.html"]
        );
    }
}
