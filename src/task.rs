extern crate gethostname;
extern crate tempdir;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempdir::TempDir;
use tokio::process;

#[cfg(test)]
use tokio::io::AsyncReadExt;

/// Describes a started task, including the process and the files it generates.
#[derive(Debug)]
pub struct Task {
    /// Handle to the running process, to be monitored by the caller.
    pub child: tokio::process::Child,
    /// One file name per entry in 'output_args' to be read to consume generated output. Empty if no
    /// output file are to be generated.
    pub output_files: Vec<PathBuf>,
    /// A resource manager for the directory containing the generated output. Once
    /// this instance is dropped all of the files are deleted. None if no output files are to be generated.
    pub output_dir: Option<TempDir>,
}

impl Task {
    /// Starts a process to execute the specified task
    ///
    /// A task is described by the directory it should run i the command to execute, and the arguments
    /// to that command. Output files named in the arguments can be remapped to *nix pipes to allow the
    /// the parent process to be returned to the server.
    ///
    /// # Arguments
    /// * `cwd` - Absolute path to the directory the command should run in
    /// * `cmd` - Absolute or relative path to the executable to run; relative to `cwd`
    /// * `args` - Zero or more command line arguments to pass to `cmd`; do not include the executable name
    /// * `output_args` - Zero or indexes into `args` indicating the argument is the name of an output
    /// * `env` - Map of zero or more environment variables to be set prior to executing the command
    ///   file to be generated
    pub fn start(cwd: &Path, cmd: &Path, mut args: Vec<String>, output_args: &[u16], env: &HashMap<String, String>) -> Result<Task, String> {
        if !cwd.exists() {
            Err(format!(
                "Execution directory {} does not exist on host {}",
                cwd.to_string_lossy(),
                hostname()
            ))
        } else if !cmd.exists() {
            Err(format!("Command {} does not exist on host {}", cmd.to_string_lossy(), hostname()))
        } else {
            let (pipe_dir, output_files) = redirect_outputs(&mut args, output_args)?;
            let child = start_task(cwd, cmd, &args, env);
            Ok(Task {
                child,
                output_files,
                output_dir: pipe_dir,
            })
        }
    }
}

/// Replace the names of output files in `args` with the names of temporary output files
///
/// args is mutated such that each element specified in output_args receives the path to a unique file
/// object to be written to. The file paths are returned as the result, along with a temporary
/// directory object which manages the lifetime of the pipes in a common directory.
///
/// # Arguments
/// * `args` - Set of command line arguments, those arguments specifying files to be generated will have
/// the paths replaced
/// * `output_args` - Indexes into `args` specifying which arguments are output file names
fn redirect_outputs(args: &mut Vec<String>, output_args: &[u16]) -> Result<(Option<TempDir>, Vec<PathBuf>), String> {
    let mut output_files: Vec<PathBuf> = Vec::with_capacity(output_args.len());
    if output_args.is_empty() {
        Ok((None, output_files))
    } else {
        let output_dir = TempDir::new("spraycc").unwrap();
        let mut taken_names = std::collections::HashSet::new();
        for i in output_args {
            let path = generate_unique_file(&output_dir, &mut taken_names, &args[*i as usize]);
            args[*i as usize] = path.to_string_lossy().to_string();
            output_files.push(path);
        }
        Ok((Some(output_dir), output_files))
    }
}

/// Create a file under a unique name in a given temporary directory
///
/// The generated file name is based on the base_name provided, and uniquified against the `taken_names`
/// hash set. `output_dir` is assumed to be empty except for the entries in `taken_names`.
fn generate_unique_file(output_dir: &TempDir, taken_names: &mut HashSet<String>, output_name: &str) -> PathBuf {
    // TODO: All this complexity to preserve the output name. Better to just use "output.N" or something?
    let output_pb = PathBuf::from(output_name);
    let base_name = output_pb.file_name().map_or(std::ffi::OsString::from("unknown"), |s| s.to_os_string());

    let mut unique_name = base_name.to_string_lossy().to_string();
    let mut idx = 1;
    while taken_names.contains(&unique_name) {
        unique_name = base_name.to_string_lossy().to_string();
        unique_name.push('.');
        unique_name.push_str(&idx.to_string());
        idx += 1;
    }
    let res = output_dir.path().join(Path::new(&unique_name));
    taken_names.insert(unique_name);
    res
}

/// Starts a task in the specified directory and retuns the child process object.
fn start_task(dir: &Path, cmd: &Path, args: &[String], env: &HashMap<String, String>) -> process::Child {
    process::Command::new(cmd)
        .args(args[1..].iter())
        .envs(env)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|_| panic!("Failed to start process {}", cmd.to_string_lossy()))
}

/// Hostname of the
fn hostname() -> String {
    // Note using 'unwrap_or_else' is lazy so 'unknown' is only allocated
    gethostname::gethostname().into_string().unwrap_or_else(|_| String::from("unknown"))
}

/// Can we start a basic task and read the output?  *nix only
#[cfg(test)]
#[tokio::test]
#[cfg(not(windows))]
async fn task_start_task() {
    let mut env = HashMap::<String, String>::new();
    env.insert(String::from("PERSON"), String::from("Mom"));
    let mut child = start_task(
        Path::new("."),
        Path::new("sh"),
        &vec![String::from("sh"), String::from("-c"), String::from("echo \"Hi ${PERSON}\"")],
        &env,
    );
    match child.wait().await {
        Ok(status) if status.success() => {
            println!("Echo ran correctly");
        }
        Ok(status) => {
            if let Some(exit_code) = status.code() {
                println!("Echo failed with exit code {}", exit_code);
            } else {
                println!("Echo terminated with signal or maybe didn't start?");
            }
            panic!();
        }
        Err(err) => {
            println!("Error: {}", err);
            panic!("Can't run echo");
        }
    }

    let mut stdout = child.stdout.expect("Unable to read stdout from echo");
    let mut buf = [0; 1024];
    let n = stdout.read(&mut buf[..]).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&buf[0..n]), "Hi Mom\n");
}

/// Check that file names are actually unique
#[test]
fn task_unique_paths() {
    let pipe_path;

    {
        let pipe_dir = TempDir::new("spraycc").unwrap();
        pipe_path = pipe_dir.path().to_path_buf();

        println!("Temporary directory is: {:?}", pipe_dir.path());
        let mut taken_names = std::collections::HashSet::new();

        let path = generate_unique_file(&pipe_dir, &mut taken_names, "notunique");
        let path2 = generate_unique_file(&pipe_dir, &mut taken_names, "notunique");
        assert_ne!(path, path2);
        let path3 = generate_unique_file(&pipe_dir, &mut taken_names, "unique");
        assert_ne!(path, path3);
    }
    // Files should all be closed, and pipe_dir dropped so all should be cleaned up
    assert!(!pipe_path.exists(), "Temporary directory {:?} was not deleted", pipe_path);
}
