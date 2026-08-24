//! C++ compiler discovery and invocation.
//!
//! Order of preference is g++ -> clang++ -> MSVC, because competitive
//! programmers overwhelmingly target GNU G++ (that is what Codeforces runs)
//! and local behaviour should match the judge where possible.
//!
//! The MSVC path is the interesting one: `cl.exe` is not on PATH unless you
//! are inside a Developer Command Prompt, which is why most tools in this
//! space simply fail on a stock Windows box. We locate the install with
//! `vswhere.exe` and bootstrap the environment through `vcvars64.bat`.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Gnu,
    Clang,
    Msvc,
}

#[derive(Clone, Debug)]
pub struct Compiler {
    pub kind: Kind,
    pub path: PathBuf,
    /// Set only for MSVC discovered outside a developer shell.
    pub vcvars: Option<PathBuf>,
}

impl Compiler {
    pub fn label(&self) -> String {
        let name = match self.kind {
            Kind::Gnu => "g++",
            Kind::Clang => "clang++",
            Kind::Msvc => "cl",
        };
        match &self.vcvars {
            Some(_) => format!("{name} (MSVC, auto-configured)"),
            None => name.to_string(),
        }
    }

    fn exe_for(&self, role: &str, out_dir: &Path) -> PathBuf {
        out_dir.join(format!("ccmin-{role}{}", std::env::consts::EXE_SUFFIX))
    }

    /// Compile every source in one go.
    ///
    /// This exists rather than a simple `compile()` loop because `vcvars64.bat`
    /// takes several seconds, and paying that once instead of once per file is
    /// the difference between a 5-second and a 15-second first run.
    pub fn compile_all(
        &self,
        targets: &[(String, PathBuf)],
        out_dir: &Path,
    ) -> Result<Vec<PathBuf>, String> {
        match self.kind {
            Kind::Gnu | Kind::Clang => targets
                .iter()
                .map(|(role, src)| self.compile_gnu(role, src, out_dir))
                .collect(),
            Kind::Msvc => self.compile_msvc(targets, out_dir),
        }
    }

    fn compile_gnu(&self, role: &str, src: &Path, out_dir: &Path) -> Result<PathBuf, String> {
        let exe = self.exe_for(role, out_dir);
        let output = Command::new(&self.path)
            .arg("-std=c++20")
            .arg("-O2")
            .arg("-o")
            .arg(&exe)
            .arg(src)
            .output()
            .map_err(|e| format!("failed to launch {}: {e}", self.path.display()))?;

        if !output.status.success() || !exe.exists() {
            return Err(format!(
                "compiling {}:\n{}",
                src.display(),
                diagnostics(&output.stdout, &output.stderr, output.status.code())
            ));
        }
        Ok(exe)
    }

    fn compile_msvc(
        &self,
        targets: &[(String, PathBuf)],
        out_dir: &Path,
    ) -> Result<Vec<PathBuf>, String> {
        // Passing a compound command to `cmd /C` as one argument runs afoul of
        // cmd's quote-stripping rules and silently loses the diagnostics, so we
        // write a batch file instead.
        let mut body = String::from("@echo off\r\n");
        if let Some(vcvars) = &self.vcvars {
            body.push_str(&format!("call \"{}\" >nul 2>&1\r\n", vcvars.display()));
        }

        let mut exes = Vec::with_capacity(targets.len());
        for (role, src) in targets {
            let exe = self.exe_for(role, out_dir);
            // Name the object file explicitly. Passing a *directory* would need
            // a trailing backslash, which escapes the closing quote and makes
            // cl parse a mangled path.
            let obj = out_dir.join(format!("ccmin-{role}.obj"));
            body.push_str(&format!(
                "\"{}\" /nologo /std:c++20 /EHsc /O2 \"{}\" /Fe:\"{}\" /Fo:\"{}\"\r\n",
                self.path.display(),
                src.display(),
                exe.display(),
                obj.display()
            ));
            body.push_str("if errorlevel 1 exit /b 1\r\n");
            exes.push(exe);
        }

        let script = out_dir.join("__ccmin_build.bat");
        std::fs::write(&script, body).map_err(|e| format!("cannot write build script: {e}"))?;
        let output = Command::new("cmd")
            .arg("/C")
            .arg(&script)
            .output()
            .map_err(|e| format!("failed to launch cmd for MSVC: {e}"))?;
        let _ = std::fs::remove_file(&script);

        let missing = exes.iter().find(|e| !e.exists());
        if !output.status.success() || missing.is_some() {
            return Err(format!(
                "compiling with MSVC:\n{}",
                diagnostics(&output.stdout, &output.stderr, output.status.code())
            ));
        }
        Ok(exes)
    }
}

fn diagnostics(stdout: &[u8], stderr: &[u8], code: Option<i32>) -> String {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    let mut msg = String::new();
    for part in [err.trim_end(), out.trim_end()] {
        if !part.trim().is_empty() {
            if !msg.is_empty() {
                msg.push('\n');
            }
            msg.push_str(part);
        }
    }
    if msg.is_empty() {
        msg = format!("compiler produced no diagnostics (exit {code:?})");
    }
    msg
}

pub fn detect() -> Result<Compiler, String> {
    for (name, kind) in [
        ("g++", Kind::Gnu),
        ("clang++", Kind::Clang),
        ("cl", Kind::Msvc),
    ] {
        if let Some(path) = which(name) {
            return Ok(Compiler {
                kind,
                path,
                vcvars: None,
            });
        }
    }

    #[cfg(windows)]
    if let Some(c) = detect_msvc_via_vswhere() {
        return Ok(c);
    }

    Err(no_compiler_help())
}

fn no_compiler_help() -> String {
    if cfg!(windows) {
        "no C++ compiler found.\n\n\
         Install one of:\n  \
         - MinGW-w64 g++  (recommended: matches Codeforces' GNU G++)\n      \
         winget install BrechtSanders.WinLibs.POSIX.UCRT\n  \
         - Visual Studio Build Tools (ccmin will auto-configure it)\n      \
         winget install Microsoft.VisualStudio.2022.BuildTools"
            .to_string()
    } else {
        "no C++ compiler found. Install g++ or clang++ and try again.".to_string()
    }
}

#[cfg(windows)]
fn detect_msvc_via_vswhere() -> Option<Compiler> {
    let program_files =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let vswhere =
        PathBuf::from(&program_files).join("Microsoft Visual Studio\\Installer\\vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }

    let out = Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    let install = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if install.is_empty() {
        return None;
    }
    let root = PathBuf::from(install);

    let vcvars = root.join("VC\\Auxiliary\\Build\\vcvars64.bat");
    if !vcvars.is_file() {
        return None;
    }

    // Pick the highest-numbered toolset under VC\Tools\MSVC.
    let toolsets = root.join("VC\\Tools\\MSVC");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&toolsets)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    let newest = versions.pop()?;

    let cl = newest.join("bin\\Hostx64\\x64\\cl.exe");
    if !cl.is_file() {
        return None;
    }

    Some(Compiler {
        kind: Kind::Msvc,
        path: cl,
        vcvars: Some(vcvars),
    })
}

/// Look up an executable on PATH, honouring PATHEXT-style suffixes on Windows.
fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".bat", ".cmd", ""]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_names_are_based_on_role_not_source_stem() {
        let compiler = Compiler {
            kind: Kind::Gnu,
            path: PathBuf::from("g++"),
            vcvars: None,
        };
        let out = Path::new("build");
        assert_ne!(compiler.exe_for("sol", out), compiler.exe_for("brute", out));
        assert!(compiler
            .exe_for("sol", out)
            .ends_with(format!("ccmin-sol{}", std::env::consts::EXE_SUFFIX)));
    }
}
