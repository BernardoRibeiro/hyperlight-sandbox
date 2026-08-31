//! A deliberately small, deterministic Bash-compatible command executor.
//!
//! This is the Phase 3 command core.  It deliberately dispatches only the
//! built-ins below and operates through [`CapFs`]; it never invokes a host
//! shell or executable.

use std::collections::BTreeMap;

use crate::{CapFs, ExecutionResult, OpenFlags};

const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_NODES: usize = 10_000;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Stateful executor for the initial bounded shell subset.
#[derive(Debug)]
pub struct Toolbox {
    cwd: String,
    environment: BTreeMap<String, String>,
    last_status: i32,
}

impl Default for Toolbox {
    fn default() -> Self {
        Self { cwd: "/work".into(), environment: BTreeMap::new(), last_status: 0 }
    }
}

impl Toolbox {
    /// Execute simple commands joined by `;`, `&&`, or `||`.
    pub fn execute_cli(&mut self, fs: &mut CapFs, script: &str) -> ExecutionResult {
        if script.len() > MAX_SCRIPT_BYTES {
            return result(2, "", "toolbox: script exceeds 64 KiB limit\n");
        }
        let parsed = match parse(script) {
            Ok(parsed) => parsed,
            Err(error) => return result(2, "", &format!("toolbox: {error}\n")),
        };
        if parsed.len() > MAX_NODES { return result(2, "", "toolbox: AST node limit exceeded\n"); }
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut previous = 0;
        for (join, words) in parsed {
            let run = match join { Join::Always => true, Join::And => previous == 0, Join::Or => previous != 0 };
            if run {
                let (status, out, err) = self.command(fs, &words);
                previous = status;
                self.last_status = status;
                append(&mut stdout, &out); append(&mut stderr, &err);
            }
        }
        result(previous, &stdout, &stderr)
    }

    fn command(&mut self, fs: &mut CapFs, words: &[String]) -> (i32, String, String) {
        if words.is_empty() { return (0, String::new(), String::new()); }
        let words: Vec<String> = words.iter().map(|word| self.expand(word)).collect();
        match words[0].as_str() {
            "true" => ok(""), "false" => (1, String::new(), String::new()),
            "echo" => ok(&(words[1..].join(" ") + "\n")),
            "printf" => ok(&words[1..].join("")),
            "pwd" => ok(&(self.cwd.clone() + "\n")),
            "cd" => { let path = words.get(1).map(String::as_str).unwrap_or("/work"); match self.path(path) { Ok(path) if self.is_directory(fs, &path) => { self.cwd = path; ok("") }, Ok(_) => fail("cd: not a directory"), Err(e) => fail(&e) } }
            "cat" => {
                let mut out = String::new();
                for path in &words[1..] { match self.path(path).and_then(|p| fs.read_guest_file(&p).map_err(|e| e.to_string())) { Ok(data) => append(&mut out, &String::from_utf8_lossy(&data)), Err(e) => return fail(&format!("cat: {e}")) } }
                ok(&out)
            }
            "touch" => match words.get(1).ok_or_else(|| "touch: missing file".to_string()).and_then(|p| self.open(fs, p, OpenFlags::CREATE).map(|_| ())) { Ok(()) => ok(""), Err(e) => fail(&e) },
            "mkdir" => match words.get(1).ok_or_else(|| "mkdir: missing directory".to_string()).and_then(|p| self.mutate(fs, p, |fs, fd, p| fs.create_directory_at(fd, p))) { Ok(()) => ok(""), Err(e) => fail(&e) },
            "rm" => match words.get(1).ok_or_else(|| "rm: missing file".to_string()).and_then(|p| self.mutate(fs, p, |fs, fd, p| fs.unlink_file_at(fd, p))) { Ok(()) => ok(""), Err(e) => fail(&e) },
            _ => (127, String::new(), format!("{}: command not found\n", words[0])),
        }
    }

    fn expand(&self, word: &str) -> String {
        let mut value = word.replace("$?", &self.last_status.to_string());
        for (name, val) in &self.environment { value = value.replace(&format!("${name}"), val); }
        value
    }
    fn path(&self, path: &str) -> Result<String, String> {
        let path = if path.starts_with('/') { path.to_owned() } else { format!("{}/{}", self.cwd, path) };
        if path.contains("..") || path.contains('\\') || path.contains('\0') { Err("invalid path".into()) } else { Ok(path) }
    }
    fn split_path(&self, path: &str) -> Result<(String, String), String> {
        let path = self.path(path)?; let (root, rest) = path.trim_start_matches('/').split_once('/').ok_or("path must name a file")?;
        Ok((format!("/{root}"), rest.into()))
    }
    /// True if `path` (already normalized via [`Self::path`]) names either a
    /// preopened mount root or a directory somewhere beneath one.
    fn is_directory(&self, fs: &CapFs, path: &str) -> bool {
        if fs.dir_by_guest_path(path).is_some() { return true; }
        match self.split_path(path) {
            Ok((root, rest)) => fs.preopens().into_iter().find(|(_, p)| *p == root).and_then(|(fd, _)| fs.stat_at(fd, &rest).ok()).is_some_and(|stat| stat.descriptor_type == crate::DescriptorType::Directory),
            Err(_) => false,
        }
    }
    fn open(&self, fs: &mut CapFs, path: &str, flags: OpenFlags) -> Result<u32, String> {
        let (root, rest) = self.split_path(path)?; let fd = fs.preopens().into_iter().find(|(_, p)| *p == root).map(|(fd, _)| fd).ok_or("unmounted path")?;
        fs.open_at(fd, &rest, flags).map_err(|e| format!("filesystem error: {e:?}"))
    }
    fn mutate<F>(&self, fs: &mut CapFs, path: &str, operation: F) -> Result<(), String>
    where F: FnOnce(&mut CapFs, u32, &str) -> Result<(), crate::FsError> {
        let (root, rest) = self.split_path(path)?; let fd = fs.preopens().into_iter().find(|(_, p)| *p == root).map(|(fd, _)| fd).ok_or("unmounted path")?;
        operation(fs, fd, &rest).map_err(|e| format!("filesystem error: {e:?}"))
    }
}

fn result(exit_code: i32, stdout: &str, stderr: &str) -> ExecutionResult { ExecutionResult { exit_code, stdout: stdout.into(), stderr: stderr.into() } }
fn ok(stdout: &str) -> (i32, String, String) { (0, stdout.into(), String::new()) }
fn fail(error: &str) -> (i32, String, String) { (1, String::new(), format!("toolbox: {error}\n")) }
fn append(target: &mut String, value: &str) { if target.len() < MAX_OUTPUT_BYTES { target.push_str(&value[..value.len().min(MAX_OUTPUT_BYTES - target.len())]); } }

#[derive(Clone, Copy)] enum Join { Always, And, Or }
fn parse(input: &str) -> Result<Vec<(Join, Vec<String>)>, &'static str> {
    let mut commands = Vec::new(); let mut words = Vec::new(); let mut word = String::new(); let mut quote = None; let mut join = Join::Always; let bytes = input.as_bytes(); let mut i = 0;
    while i < bytes.len() { let c = bytes[i] as char; if let Some(q) = quote { if c == q { quote = None } else { word.push(c) }; i += 1; continue; }
        match c { '\'' | '"' => quote = Some(c), '\\' => { i += 1; if i == bytes.len() { return Err("trailing escape") }; word.push(bytes[i] as char) }, ' ' | '\t' | '\n' => { if !word.is_empty() { words.push(std::mem::take(&mut word)); } }, ';' => { if !word.is_empty() { words.push(std::mem::take(&mut word)); } if !words.is_empty() { commands.push((join, std::mem::take(&mut words))); join = Join::Always; } }, '&' | '|' if i + 1 < bytes.len() && bytes[i + 1] == bytes[i] => { if !word.is_empty() { words.push(std::mem::take(&mut word)); } if words.is_empty() { return Err("operator without command") }; commands.push((join, std::mem::take(&mut words))); join = if c == '&' { Join::And } else { Join::Or }; i += 1 }, _ => word.push(c) }; i += 1;
    }
    if quote.is_some() { return Err("unterminated quote") }; if !word.is_empty() { words.push(word) }; if !words.is_empty() { commands.push((join, words)); }; Ok(commands)
}

#[cfg(test)] mod tests { use super::*; use crate::WorkDirAccess;
    #[test] fn runs_composition_quotes_and_status() { let mut shell = Toolbox::default(); let mut fs = CapFs::new(); let result = shell.execute_cli(&mut fs, "false && echo no || echo 'yes yes'; echo $?"); assert_eq!(result.stdout, "yes yes\n0\n"); }
    #[test] fn mutations_use_capfs_permissions() { let dir = tempfile::tempdir().unwrap(); let mut fs = CapFs::new().with_work(dir.path(), WorkDirAccess::ReadWrite).unwrap(); let mut shell = Toolbox::default(); assert_eq!(shell.execute_cli(&mut fs, "mkdir nested; touch nested/a; cat nested/a").exit_code, 0); assert!(dir.path().join("nested/a").exists()); }
    #[test] fn cd_into_created_subdirectory() { let dir = tempfile::tempdir().unwrap(); let mut fs = CapFs::new().with_work(dir.path(), WorkDirAccess::ReadWrite).unwrap(); let mut shell = Toolbox::default(); let result = shell.execute_cli(&mut fs, "mkdir project && cd project && pwd"); assert_eq!(result.exit_code, 0); assert_eq!(result.stdout, "/work/project\n"); assert_eq!(shell.execute_cli(&mut fs, "cd missing").exit_code, 1); }
    #[test] fn exit_status_tracks_within_composed_script() { let dir = tempfile::tempdir().unwrap(); let mut shell = Toolbox::default(); let mut fs = CapFs::new().with_work(dir.path(), WorkDirAccess::ReadWrite).unwrap(); let result = shell.execute_cli(&mut fs, "cat missing.txt; echo $?"); assert_eq!(result.stdout, "1\n"); }
}
