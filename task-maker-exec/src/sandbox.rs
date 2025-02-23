use failure::Error;
use itertools::Itertools;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use task_maker_dag::*;
use task_maker_store::*;
use tempdir::TempDir;

/// The list of all the system-wide readable directories inside the sandbox.
const READABLE_DIRS: &[&str] = &[
    "/lib",
    "/lib64",
    "/usr",
    "/bin",
    "/opt",
    // update-alternatives stuff, sometimes the executables are symlinked here
    "/etc/alternatives/",
    "/var/lib/dpkg/alternatives/",
];

/// Result of the execution of the sandbox.
#[derive(Debug)]
pub enum SandboxResult {
    /// The sandbox exited successfully, the statistics about the sandboxed process are reported.
    Success {
        /// The exit status of the process.
        exit_status: u32,
        /// The signal that caused the process to exit.
        signal: Option<u32>,
        /// Resources used by the process.
        resources: ExecutionResourcesUsage,
        /// Whether the sandbox killed the process.
        was_killed: bool,
    },
    /// The sandbox failed to execute the process, an error message is reported. Note that this
    /// represents a sandbox error, not the process failure.
    Failed {
        /// The error reported by the sandbox.
        error: String,
    },
}

/// Internals of the sandbox.
#[derive(Debug)]
struct SandboxData {
    /// Handle to the temporary directory, will be deleted on drop. It's always Some(_) except
    /// inside `Drop`.
    boxdir: Option<TempDir>,
    /// Whether to keep the sandbox after exit.
    keep_sandbox: bool,
}

/// Wrapper around the sandbox. Cloning this struct will keep the reference of the same sandbox,
/// keeping the content alive.
///
/// This sandbox works only on Unix systems because it needs to set the executable bit on some
/// files.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Internal data of the sandbox.
    data: Arc<Mutex<SandboxData>>,
    /// Execution to run.
    execution: Execution,
}

/// The outcome from `tmbox`. If the sandbox fails to run only `error` and `message` are set,
/// otherwise all the fields are present except for `message`.
#[derive(Debug, Deserialize)]
struct TMBoxResult {
    /// Whether the sandbox failed to execute the subprocess, will set `message`.
    error: bool,
    /// Error message from the sandbox.
    message: Option<String>,
    /// Total CPU time in user space, in seconds.
    cpu_time: Option<f64>,
    /// Total CPU time in kernel space, in seconds.
    sys_time: Option<f64>,
    /// Total time from the start to the end of the process, in seconds.
    wall_time: Option<f64>,
    /// Peak memory usage of the process in KiB.
    memory_usage: Option<u64>,
    /// Exit status code of the process.
    status_code: Option<u32>,
    /// Signal that made the process exit.
    signal: Option<u32>,
    /// Whether the sandbox killed the process.
    killed_by_sandbox: Option<bool>,
}

impl Sandbox {
    /// Make a new sandbox for the specified execution, copying all the required files. To start the
    /// sandbox call `run`.
    pub fn new(
        sandboxes_dir: &Path,
        execution: &Execution,
        dep_keys: &HashMap<FileUuid, FileStoreHandle>,
    ) -> Result<Sandbox, Error> {
        std::fs::create_dir_all(sandboxes_dir)?;
        let boxdir = TempDir::new_in(sandboxes_dir, "box")?;
        Sandbox::setup(boxdir.path(), execution, dep_keys)?;
        Ok(Sandbox {
            data: Arc::new(Mutex::new(SandboxData {
                boxdir: Some(boxdir),
                keep_sandbox: false,
            })),
            execution: execution.clone(),
        })
    }

    /// Starts the sandbox and blocks the thread until the sandbox exits.
    pub fn run(&self) -> Result<SandboxResult, Error> {
        let boxdir = self.data.lock().unwrap().path().to_owned();
        trace!("Running sandbox at {:?}", boxdir);
        let tmbox_path = Path::new(env!("OUT_DIR")).join("bin").join("tmbox");
        let tmbox_path = if tmbox_path.exists() {
            tmbox_path
        } else {
            "tmbox".into()
        };
        let mut sandbox = Command::new(tmbox_path);
        let command = match self.build_command(&boxdir) {
            Ok(cmd) => cmd,
            Err(e) => return Ok(SandboxResult::Failed { error: e }),
        };
        sandbox.args(command);
        trace!("Sandbox command: {:?}", sandbox);
        let res = sandbox.output()?;
        trace!("Sandbox output: {:?}", res);
        let outcome = serde_json::from_str::<TMBoxResult>(std::str::from_utf8(&res.stdout)?)?;
        if outcome.error {
            Ok(SandboxResult::Failed {
                error: outcome
                    .message
                    .unwrap_or_else(|| "No output from sandbox".into()),
            })
        } else {
            let signal = if outcome.signal.unwrap() == 0 {
                None
            } else {
                Some(outcome.signal.unwrap())
            };
            Ok(SandboxResult::Success {
                exit_status: outcome.status_code.unwrap(),
                signal,
                resources: ExecutionResourcesUsage {
                    cpu_time: outcome.cpu_time.unwrap(),
                    sys_time: outcome.sys_time.unwrap(),
                    wall_time: outcome.wall_time.unwrap(),
                    memory: outcome.memory_usage.unwrap(),
                },
                was_killed: outcome.killed_by_sandbox.unwrap(),
            })
        }
    }

    /// Tell the sandbox process to kill the underlying process, this will make `run` terminate more
    /// quickly.
    pub fn kill(&self) {
        info!(
            "Sandbox at {:?} got killed",
            self.data.lock().unwrap().path()
        );
        unimplemented!();
    }

    /// Make the sandbox persistent, the sandbox directory won't be deleted after the execution.
    pub fn keep(&mut self) {
        let mut data = self.data.lock().unwrap();
        let path = data
            .boxdir
            .as_ref()
            .expect("Box dir has gone?!?")
            .path()
            .to_owned();
        debug!("Keeping sandbox at {:?}", path);
        data.keep_sandbox = true;
        let serialized =
            serde_json::to_string_pretty(&self.execution).expect("Cannot serialize execution");
        std::fs::write(path.join("info.json"), serialized)
            .expect("Cannot write execution info inside sandbox");
        if let Ok(command) = self.build_command(&path) {
            let command = command.into_iter().map(|s| format!("{:?}", s)).join(" ");
            std::fs::write(path.join("command.txt"), format!("tmbox {}\n", command))
                .expect("Cannot write command info inside sandbox");
        }
    }

    /// Path of the file where the standard output is written to.
    pub fn stdout_path(&self) -> PathBuf {
        self.data.lock().unwrap().path().join("stdout")
    }

    /// Path of the file where the standard error is written to.
    pub fn stderr_path(&self) -> PathBuf {
        self.data.lock().unwrap().path().join("stderr")
    }

    /// Path of the file where that output file is written to.
    pub fn output_path(&self, output: &Path) -> PathBuf {
        self.data.lock().unwrap().path().join("box").join(output)
    }

    /// Build the command line arguments of `tmbox`.
    fn build_command(&self, boxdir: &Path) -> Result<Vec<OsString>, String> {
        let mut args: Vec<OsString> = vec![];
        args.push("--directory".into());
        args.push(boxdir.join("box").into());
        args.push("--json".into());
        args.push("--env".into());
        args.push("PATH".into());
        if self.execution.stdin.is_some() {
            args.push("--stdin".into());
            args.push(boxdir.join("stdin").into());
        } else {
            args.push("--stdin".into());
            args.push("/dev/null".into());
        }
        if self.execution.stdout.is_some() {
            args.push("--stdout".into());
            args.push(boxdir.join("stdout").into());
        } else {
            args.push("--stdout".into());
            args.push("/dev/null".into());
        }
        if self.execution.stderr.is_some() {
            args.push("--stderr".into());
            args.push(boxdir.join("stderr").into());
        } else {
            args.push("--stderr".into());
            args.push("/dev/null".into());
        }
        for (key, value) in self.execution.env.iter() {
            args.push("--env".into());
            args.push(OsString::from(format!("{}={}", key, value)));
        }
        // set the cpu_limit (--time parameter) to the sum of cpu_time and sys_time
        let cpu_limit = match (
            self.execution.limits.cpu_time,
            self.execution.limits.sys_time,
        ) {
            (Some(cpu), Some(sys)) => Some(cpu + sys),
            (Some(cpu), None) => Some(cpu),
            (None, Some(sys)) => Some(sys),
            (None, None) => None,
        };
        if let Some(cpu) = cpu_limit {
            let cpu = cpu + self.execution.config().extra_time;
            args.push("--time".into());
            args.push(cpu.to_string().into());
        }
        if let Some(wall) = self.execution.limits.wall_time {
            let wall = wall + self.execution.config().extra_time;
            args.push("--wall".into());
            args.push(wall.to_string().into());
        }
        if let Some(mem) = self.execution.limits.memory {
            args.push("--memory".into());
            args.push(mem.to_string().into());
        }
        if let Some(1) = self.execution.limits.nproc {
            // default is not multi process
        } else {
            args.push("--multiprocess".into());
        }
        // allow reading some basic directories
        for dir in READABLE_DIRS {
            if Path::new(dir).is_dir() {
                args.push("--readable-dir".into());
                args.push(dir.into());
            }
        }
        for dir in &self.execution.limits.extra_readable_dirs {
            if dir.is_dir() {
                args.push("--readable-dir".into());
                args.push(dir.into());
            }
        }
        if self.execution.limits.mount_tmpfs {
            args.push("--mount-tmpfs".into());
        }
        args.push("--".into());
        match &self.execution.command {
            ExecutionCommand::System(cmd) => {
                if let Ok(cmd) = which::which(cmd) {
                    args.push(cmd.into())
                } else {
                    return Err(format!("Executable {:?} not found", cmd));
                }
            }
            ExecutionCommand::Local(cmd) => args.push(cmd.into()),
        };
        for arg in self.execution.args.iter() {
            args.push(arg.into());
        }
        Ok(args)
    }

    /// Setup the sandbox directory with all the files required for the execution.
    fn setup<P: AsRef<Path>>(
        box_dir: P,
        execution: &Execution,
        dep_keys: &HashMap<FileUuid, FileStoreHandle>,
    ) -> Result<(), Error> {
        trace!(
            "Setting up sandbox at {:?} for '{}'",
            box_dir.as_ref(),
            execution.description
        );
        std::fs::create_dir_all(box_dir.as_ref().join("box"))?;
        if let Some(stdin) = execution.stdin {
            Sandbox::write_sandbox_file(
                &box_dir.as_ref().join("stdin"),
                dep_keys.get(&stdin).expect("stdin not provided").path(),
                false,
            )?;
        }
        if execution.stdout.is_some() {
            Sandbox::touch_file(&box_dir.as_ref().join("stdout"), 0o600)?;
        }
        if execution.stderr.is_some() {
            Sandbox::touch_file(&box_dir.as_ref().join("stderr"), 0o600)?;
        }
        for (path, input) in execution.inputs.iter() {
            Sandbox::write_sandbox_file(
                &box_dir.as_ref().join("box").join(&path),
                dep_keys.get(&input.file).expect("file not provided").path(),
                input.executable,
            )?;
        }
        for path in execution.outputs.keys() {
            Sandbox::touch_file(&box_dir.as_ref().join("box").join(&path), 0o600)?;
        }
        // remove the write bit on the box folder
        if execution.limits.read_only {
            Sandbox::set_permissions(&box_dir.as_ref().join("box"), 0o500)?;
        }
        trace!("Sandbox at {:?} ready!", box_dir.as_ref());
        Ok(())
    }

    /// Put a file inside the sandbox, creating the directories if needed and making it executable
    /// if needed.
    ///
    /// The file will have the most restrictive permissions possible:
    /// - `r--------` (0o400) if not executable.
    /// - `r-x------` (0o500) if executable.
    fn write_sandbox_file(dest: &Path, source: &Path, executable: bool) -> Result<(), Error> {
        std::fs::create_dir_all(dest.parent().expect("Invalid destination path"))?;
        std::fs::copy(source, dest)?;
        if executable {
            Sandbox::set_permissions(dest, 0o500)?;
        } else {
            Sandbox::set_permissions(dest, 0o400)?;
        }
        Ok(())
    }

    /// Create an empty file inside the sandbox and chmod-it.
    fn touch_file(dest: &Path, mode: u32) -> Result<(), Error> {
        std::fs::create_dir_all(dest.parent().expect("Invalid file path"))?;
        std::fs::File::create(dest)?;
        let mut permisions = std::fs::metadata(&dest)?.permissions();
        permisions.set_mode(mode);
        std::fs::set_permissions(dest, permisions)?;
        Ok(())
    }

    fn set_permissions(dest: &Path, perm: u32) -> Result<(), Error> {
        let mut permissions = std::fs::metadata(&dest)?.permissions();
        permissions.set_mode(perm);
        std::fs::set_permissions(dest, permissions)?;
        Ok(())
    }
}

impl SandboxData {
    fn path(&self) -> &Path {
        // this unwrap is safe since only `Drop` will remove the boxdir
        self.boxdir.as_ref().unwrap().path()
    }
}

impl Drop for SandboxData {
    fn drop(&mut self) {
        if self.keep_sandbox {
            // this will unwrap the directory, dropping the `TempDir` without deleting the directory
            self.boxdir.take().map(TempDir::into_path);
        } else if Sandbox::set_permissions(&self.boxdir.as_ref().unwrap().path().join("box"), 0o700)
            .is_err()
        {
            warn!("Cannot 'chmod 700' the sandbox directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Sandbox;
    use itertools::Itertools;
    use std::collections::HashMap;
    use std::path::Path;
    use task_maker_dag::{Execution, ExecutionCommand};

    fn assert_contains(source: &[String], check: &[&str]) {
        for i in 0..source.len() {
            if source[i] == check[0] {
                let mut valid = true;
                for j in 1..check.len() {
                    if source[i + j] != check[j] {
                        valid = false;
                        break;
                    }
                }
                if valid {
                    return;
                }
            }
        }
        panic!("{:?} does not contain {:?}", source, check);
    }

    #[test]
    fn test_remove_sandbox_on_drop() {
        let tmpdir = tempdir::TempDir::new("tm-test").unwrap();
        let mut exec = Execution::new("test", ExecutionCommand::system("true"));
        exec.output("fooo");
        exec.limits_mut().read_only(true);
        let sandbox = Sandbox::new(tmpdir.path(), &exec, &HashMap::new()).unwrap();
        let outfile = sandbox.output_path(Path::new("fooo"));
        sandbox.run().unwrap();
        drop(sandbox);
        assert!(!outfile.exists());
        assert!(!outfile.parent().unwrap().exists()); // the box/ dir
        assert!(!outfile.parent().unwrap().parent().unwrap().exists()); // the sandbox dir
    }

    #[test]
    fn test_command_args() {
        let tmpdir = tempdir::TempDir::new("tm-test").unwrap();
        let mut exec = Execution::new("test", ExecutionCommand::local("foo"));
        exec.args(vec!["bar", "baz"]);
        exec.limits_mut()
            .sys_time(1.0)
            .cpu_time(2.6)
            .wall_time(10.0)
            .mount_tmpfs(true)
            .add_extra_readable_dir("/home")
            .nproc(2)
            .memory(1234);
        exec.env("foo", "bar");
        let sandbox = Sandbox::new(tmpdir.path(), &exec, &HashMap::new()).unwrap();
        let args = sandbox
            .build_command(tmpdir.path())
            .unwrap()
            .into_iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect_vec();
        let extra_time = exec.config().extra_time;
        let total_time = 1.0 + 2.6 + extra_time;
        let wall_time = 10.0 + extra_time;
        let boxdir = tmpdir.path().join("box");
        assert_contains(&args, &["--directory", &boxdir.to_string_lossy()]);
        assert_contains(&args, &["--json"]);
        assert_contains(&args, &["--time", &total_time.to_string()]);
        assert_contains(&args, &["--wall", &wall_time.to_string()]);
        assert_contains(&args, &["--memory", "1234"]);
        assert_contains(&args, &["--readable-dir", "/home"]);
        assert_contains(&args, &["--mount-tmpfs"]);
        assert_contains(&args, &["--multiprocess"]);
        assert_contains(&args, &["--env", "foo=bar"]);
        assert_contains(&args, &["--stdin", "/dev/null"]);
        assert_contains(&args, &["--stdout", "/dev/null"]);
        assert_contains(&args, &["--stderr", "/dev/null"]);
        assert_contains(&args, &["--", "foo", "bar", "baz"]);
    }
}
