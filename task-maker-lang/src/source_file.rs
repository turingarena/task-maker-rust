use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use failure::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use task_maker_dag::*;

use crate::languages::*;
use crate::{GraderMap, LanguageManager};

/// A source file that will be able to be executed (with an optional compilation step).
///
/// After creating a `SourceFile` using `new` you can add start using it via the `execute` method.
/// Note that it may add to the DAG an extra execution for compiling the source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    /// Path to the source file.
    pub path: PathBuf,
    /// Path to the base directory (e.g. the task root), used for including the source file
    /// dependencies from the command line args of the executable (in case of relative paths).
    pub base_path: PathBuf,
    /// Language of the source file.
    #[serde(serialize_with = "language_serializer")]
    #[serde(deserialize_with = "language_deserializer")]
    language: Arc<dyn Language>,
    /// Handle to the executable after the compilation/provided file.
    executable: Arc<Mutex<Option<File>>>,
    /// An optional handler to the map of the graders.
    grader_map: Option<Arc<GraderMap>>,
    /// Where to write the compiled executable.
    write_bin_to: Option<PathBuf>,
    /// The stdout of the compilation, set if `prepare` has been called, and the language supports
    /// compilation.
    compilation_stdout: Arc<Mutex<Option<File>>>,
    /// The stderr of the compilation, set if `prepare` has been called, and the language supports
    /// compilation.
    compilation_stderr: Arc<Mutex<Option<File>>>,
}

impl SourceFile {
    /// Make a new `SourceFile` from the provided file. Will return `None` if the language is
    /// unknown.
    ///
    /// The language of the source file will be detected using the
    /// [`LanguageManager`](struct.LanguageManager.html), only those languages are supported.
    ///
    /// Because the execution/compilation may require some additional files a
    /// [`GraderMap`](struct.GraderMap.html) is required.
    pub fn new<P: Into<PathBuf>, P2: Into<PathBuf>, P3: Into<PathBuf>>(
        path: P,
        base_path: P2,
        grader_map: Option<Arc<GraderMap>>,
        write_bin_to: Option<P3>,
    ) -> Option<SourceFile> {
        let path = path.into();
        let base_path = base_path.into();
        let lang = LanguageManager::detect_language(&path);
        lang.as_ref()?;
        Some(SourceFile {
            path,
            base_path,
            language: lang.unwrap(),
            executable: Arc::new(Mutex::new(None)),
            grader_map,
            write_bin_to: write_bin_to.map(|p| p.into()),
            compilation_stdout: Arc::new(Mutex::new(None)),
            compilation_stderr: Arc::new(Mutex::new(None)),
        })
    }

    /// Execute the program relative to this source file with the specified args. If the file has
    /// not been compiled yet this may add the compilation to the DAG. The compilation is added to
    /// the DAG only once for each `SourceFile` instance.
    ///
    /// The returned execution has the following properties already set:
    /// - the list of arguments
    /// - the input file for the executable
    /// - the input files from the language runtime dependencies
    /// - the input files from the grader map runtime dependencies
    ///
    /// The first element returned is the UUID of the execution of the compilation. It will be
    /// returned only once, even if the `execute` method is called more than once. The actual
    /// execution is returned as second parameter.
    ///
    /// The returned execution has all the dependencies already set, but it **has not been added**
    /// to the DAG yet. In order for this execution to work it has to be manually added to the DAG
    /// using [`ExecutionDAG::add_execution`](../task_maker_dag/struct.ExecutionDAG.html#method.add_execution).
    ///
    /// # Examples
    ///
    /// When executing a `.cpp` file the first item returned contains an handle to the compilation
    /// execution. Note that the second time the handle is not returned.
    ///
    /// ```
    /// use task_maker_dag::ExecutionDAG;
    /// use task_maker_lang::SourceFile;
    /// # use tempdir::TempDir;
    /// # use std::path::PathBuf;
    ///
    /// # let tempdir = TempDir::new("tm-tests").unwrap();
    /// # std::fs::write(tempdir.path().join("test.cpp"), "foobar.cpp").unwrap();
    /// # let path = tempdir.path().join("test.cpp");
    /// let mut dag = ExecutionDAG::new();
    /// let mut source = SourceFile::new(path /* test.cpp */, "", None, None::<PathBuf>).unwrap();
    ///
    /// let (comp, exec) = source.execute(&mut dag, "Execution", vec!["arg1".into()]).unwrap();
    /// assert!(comp.is_some());
    /// // customize the execution...
    /// dag.add_execution(exec);
    ///
    /// let (comp, exec) = source.execute(&mut dag, "Execution 2", vec!["arg1".into()]).unwrap();
    /// assert!(comp.is_none());
    /// dag.add_execution(exec);
    /// ```
    ///
    /// When executing a `.py` file the handle is not returned.
    ///
    /// ```
    /// use task_maker_dag::ExecutionDAG;
    /// use task_maker_lang::SourceFile;
    /// # use tempdir::TempDir;
    /// # use std::path::PathBuf;
    ///
    /// # let tempdir = TempDir::new("tm-tests").unwrap();
    /// # std::fs::write(tempdir.path().join("test.py"), "foobar.cpp").unwrap();
    /// # let path = tempdir.path().join("test.py");
    /// let mut dag = ExecutionDAG::new();
    /// let mut source = SourceFile::new(path /* test.py */, "", None, None::<PathBuf>).unwrap();
    ///
    /// let (comp, exec) = source.execute(&mut dag, "Execution", vec!["arg1".into()]).unwrap();
    /// assert!(comp.is_none());
    /// // customize the execution...
    /// dag.add_execution(exec);
    /// ```
    pub fn execute<S: AsRef<str>>(
        &self,
        dag: &mut ExecutionDAG,
        description: S,
        args: Vec<String>,
    ) -> Result<(Option<ExecutionUuid>, Execution), Error> {
        let comp = self.prepare(dag)?;
        let mut exec = Execution::new(
            description.as_ref(),
            self.language.runtime_command(&self.path),
        );
        for arg in &args {
            let path = self.base_path.join(arg);
            if path.exists() {
                let file = File::new(format!(
                    "Command line dependency {:?} of {:?}",
                    path, self.path
                ));
                exec.input(&file, arg, false);
                dag.provide_file(file, path)?;
            }
        }
        exec.args(self.language.runtime_args(&self.path, args));
        exec.input(
            self.executable.lock().unwrap().as_ref().unwrap(),
            &self.language.executable_name(&self.path),
            true,
        );
        for dep in self.language.runtime_dependencies(&self.path) {
            exec.input(&dep.file, &dep.sandbox_path, dep.executable);
            dag.provide_file(dep.file, &dep.local_path)?;
        }
        if let Some(grader_map) = self.grader_map.as_ref() {
            for dep in grader_map.get_runtime_deps(self.language.as_ref()) {
                exec.input(&dep.file, &dep.sandbox_path, dep.executable);
                exec.args = self.language.runtime_add_file(exec.args, &dep.sandbox_path);
                dag.provide_file(dep.file, &dep.local_path)?;
            }
        }
        self.language.custom_limits(exec.limits_mut());
        Ok((comp, exec))
    }

    /// The file name of the source file.
    ///
    /// ```
    /// use task_maker_lang::SourceFile;
    /// use std::path::PathBuf;
    ///
    /// let source = SourceFile::new("path/to/sourcefile.cpp", "", None, None::<PathBuf>).unwrap();
    ///
    /// assert_eq!(source.name(), "sourcefile.cpp");
    /// ```
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .expect("Invalid file name")
            .to_string_lossy()
            .to_string()
    }

    /// The standard output of the compilation, if the source file is compiled and `execute` has
    /// been called at least once.
    pub fn compilation_stdout(&self) -> Option<File> {
        self.compilation_stdout.lock().unwrap().clone()
    }

    /// The standard error of the compilation, if the source file is compiled and `execute` has
    /// been called at least once.
    pub fn compilation_stderr(&self) -> Option<File> {
        self.compilation_stderr.lock().unwrap().clone()
    }

    /// Prepare the source file setting the `executable` and eventually compiling the source file.
    fn prepare(&self, dag: &mut ExecutionDAG) -> Result<Option<ExecutionUuid>, Error> {
        if self.executable.lock().unwrap().is_some() {
            return Ok(None);
        }
        if self.language.need_compilation() {
            let mut comp = Execution::new(
                &format!("Compilation of {:?}", self.name()),
                self.language.compilation_command(&self.path),
            );
            comp.tag(ExecutionTag::from("compilation"));
            comp.args = self.language.compilation_args(&self.path);
            let source = File::new(&format!("Source file of {:?}", self.path));
            comp.input(
                &source,
                Path::new(self.path.file_name().expect("Invalid file name")),
                false,
            );
            comp.limits.nproc = None;
            comp.limits.read_only(false); // the compilers may need to store some temp files
            for dep in self.language.compilation_dependencies(&self.path) {
                comp.input(&dep.file, &dep.sandbox_path, dep.executable);
                dag.provide_file(dep.file, &dep.local_path)?;
            }
            if let Some(grader_map) = self.grader_map.as_ref() {
                for dep in grader_map.get_compilation_deps(self.language.as_ref()) {
                    comp.input(&dep.file, &dep.sandbox_path, dep.executable);
                    comp.args = self
                        .language
                        .compilation_add_file(comp.args, &dep.sandbox_path);
                    dag.provide_file(dep.file, &dep.local_path)?;
                }
            }
            *self.compilation_stdout.lock().unwrap() = Some(comp.stdout());
            *self.compilation_stderr.lock().unwrap() = Some(comp.stderr());
            let exec = comp.output(&self.language.executable_name(&self.path));
            let comp_uuid = comp.uuid;
            dag.add_execution(comp);
            dag.provide_file(source, &self.path)?;
            if dag.config_mut().copy_exe {
                if let Some(write_bin_to) = &self.write_bin_to {
                    dag.write_file_to(&exec, write_bin_to, true);
                }
            }
            *self.executable.lock().unwrap() = Some(exec);
            Ok(Some(comp_uuid))
        } else {
            let executable = File::new(&format!("Source file of {:?}", self.path));
            if dag.config_mut().copy_exe {
                if let Some(write_bin_to) = &self.write_bin_to {
                    dag.write_file_to(&executable, write_bin_to, true);
                }
            }
            *self.executable.lock().unwrap() = Some(executable.clone());
            dag.provide_file(executable, &self.path)?;
            Ok(None)
        }
    }
}

/// Serializer for `Arc<dyn Language>`. It serializes just the name of the language, expecting the
/// deserializer to know how to deserialize it.
fn language_serializer<S>(lang: &Arc<dyn Language>, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    ser.serialize_str(lang.name())
}

/// Deserializer for `Arc<dyn Language>`. It expects a `String` to be deserialized, searching in the
/// `LanguageManager` know languages the instance of that language.
fn language_deserializer<'de, D>(deser: D) -> Result<Arc<dyn Language>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let lang_name = String::deserialize(deser)?;
    Ok(
        LanguageManager::from_name(lang_name)
            .ok_or_else(|| D::Error::custom("unknown language"))?,
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tempdir::TempDir;

    use task_maker_exec::eval_dag_locally;

    use super::*;

    #[test]
    fn test_source_file_cpp() {
        let cwd = TempDir::new("tm-test").unwrap();

        let mut dag = ExecutionDAG::new();
        dag.config_mut().copy_exe(true);

        let source = "int main() {return 0;}";
        let source_path = cwd.path().join("source.cpp");
        std::fs::File::create(&source_path)
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let source = SourceFile::new(&source_path, "", None, Some(cwd.path().join("bin"))).unwrap();
        let (comp, exec) = source.execute(&mut dag, "Testing exec", vec![]).unwrap();
        assert!(comp.is_some());

        let exec_start = Arc::new(AtomicBool::new(false));
        let exec_start2 = exec_start.clone();
        let exec_done = Arc::new(AtomicBool::new(false));
        let exec_done2 = exec_done.clone();
        let exec_skipped = Arc::new(AtomicBool::new(false));
        let exec_skipped2 = exec_skipped.clone();
        dag.on_execution_start(&exec.uuid, move |_w| {
            exec_start2.store(true, Ordering::Relaxed);
            Ok(())
        });
        dag.on_execution_done(&exec.uuid, move |_res| {
            exec_done2.store(true, Ordering::Relaxed);
            Ok(())
        });
        dag.on_execution_skip(&exec.uuid, move || {
            exec_skipped2.store(true, Ordering::Relaxed);
            Ok(())
        });
        dag.add_execution(exec);

        eval_dag_locally(dag, cwd.path(), 2, cwd.path());

        assert!(exec_start.load(Ordering::Relaxed));
        assert!(exec_done.load(Ordering::Relaxed));
        assert!(!exec_skipped.load(Ordering::Relaxed));
        assert!(cwd.path().join("bin").exists());
    }
}
