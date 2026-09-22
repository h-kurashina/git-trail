//! Launching the user's editor. `$VISUAL`, then `$EDITOR`, then a platform
//! default. The editor runs exactly as the user configured it: no flags, no
//! init files of our own.

use std::path::Path;
use std::process::Command;

use crate::error::{Result, TrailError};

pub struct Editor {
    pub program: String,
    pub args: Vec<String>,
}

impl Editor {
    pub fn resolve() -> Result<Self> {
        for var in ["VISUAL", "EDITOR"] {
            if let Some(value) = std::env::var_os(var) {
                if let Some(editor) = Self::parse(&value.to_string_lossy(), var)? {
                    return Ok(editor);
                }
            }
        }
        let fallback = if cfg!(windows) { "notepad" } else { "vi" };
        if which(fallback) {
            return Ok(Editor {
                program: fallback.to_string(),
                args: Vec::new(),
            });
        }
        Err(TrailError::NoEditor)
    }

    /// Parse an `$EDITOR`-style value with shell word rules, so quoted
    /// arguments such as `nvim --cmd "set signcolumn=no"` survive. An empty
    /// value means "unset". Unbalanced quotes are an error.
    pub fn parse(value: &str, var: &str) -> Result<Option<Self>> {
        let words = shell_words::split(value)
            .map_err(|e| TrailError::EditorFailed(format!("cannot parse ${var}={value:?}: {e}")))?;
        let mut words = words.into_iter();
        Ok(words.next().map(|program| Editor {
            program,
            args: words.collect(),
        }))
    }

    pub fn describe(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Open `path` and wait. Stdio is inherited so terminal editors work.
    pub fn open(&self, path: &Path) -> Result<()> {
        let status = Command::new(&self.program)
            .args(&self.args)
            .arg(path)
            .status()
            .map_err(|e| {
                TrailError::EditorFailed(format!("cannot run {}: {e}", self.describe()))
            })?;
        if !status.success() {
            return Err(TrailError::EditorFailed(format!(
                "{} exited with {status}",
                self.describe()
            )));
        }
        Ok(())
    }
}

fn which(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() || (cfg!(windows) && dir.join(format!("{program}.exe")).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_arguments() {
        let e = Editor::parse(r#"nvim --cmd "set signcolumn=no" -f"#, "EDITOR")
            .unwrap()
            .unwrap();
        assert_eq!(e.program, "nvim");
        assert_eq!(e.args, vec!["--cmd", "set signcolumn=no", "-f"]);
        let e = Editor::parse("code --wait", "VISUAL").unwrap().unwrap();
        assert_eq!(e.args, vec!["--wait"]);
        assert!(Editor::parse("   ", "EDITOR").unwrap().is_none());
        assert!(Editor::parse(r#"nvim --cmd "unbalanced"#, "EDITOR").is_err());
    }
}
