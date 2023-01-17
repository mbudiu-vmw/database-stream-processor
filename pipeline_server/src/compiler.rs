use crate::{ProjectDB, ProjectId, ProjectStatus, ServerConfig, Version};
use anyhow::{Error as AnyError, Result as AnyResult};
use fs_extra::{dir, dir::CopyOptions};
use log::{debug, error, trace};
use std::{
    process::{ExitStatus, Stdio},
    sync::Arc,
};
use tokio::{
    fs,
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    process::{Child, Command},
    select, spawn,
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, Duration},
};

const COMPILER_POLL_INTERVAL: Duration = Duration::from_millis(1000);

pub struct Compiler {
    // config: CompilerConfig,
    // command_sender: Sender<CompilerCommand>,
    compiler_task: JoinHandle<AnyResult<()>>,
}

const MAIN_FUNCTION: &str = r#"
fn main() {
    dbsp_adapters::server::server_main(&circuit).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
}"#;

impl Compiler {
    pub(crate) async fn new(config: &ServerConfig, db: Arc<Mutex<ProjectDB>>) -> AnyResult<Self> {
        // let (command_sender, command_receiver) = channel(100);
        fs::create_dir_all(&config.workspace_dir())
            .await
            .map_err(|e| {
                AnyError::msg(format!(
                    "failed to create Rust workspace directory '{}': {e}",
                    config.workspace_dir().display()
                ))
            })?;

        let mut copy_options = CopyOptions::new();
        copy_options.overwrite = true;
        copy_options.copy_inside = true;
        dir::copy(config.sql_lib_path(), config.workspace_dir(), &copy_options)?;

        let compiler_task = spawn(Self::compiler_task(config.clone(), db));
        Ok(Self {
            //command_sender,
            compiler_task,
        })
    }

    async fn compiler_task(config: ServerConfig, db: Arc<Mutex<ProjectDB>>) -> AnyResult<()> {
        Self::do_compiler_task(config, db).await.map_err(|e| {
            error!("compiler task failed; error: '{e}'");
            e
        })
    }

    async fn do_compiler_task(
        /* command_receiver: Receiver<CompilerCommand>, */ config: ServerConfig,
        db: Arc<Mutex<ProjectDB>>,
    ) -> AnyResult<()> {
        let mut job: Option<CompilationJob> = None;

        loop {
            select! {
                _ = sleep(COMPILER_POLL_INTERVAL) => {
                    let mut cancel = false;
                    if let Some(job) = &job {
                        let descr = db.lock().await.get_project(job.project_id).await?;
                        if let Some(descr) = descr {
                            if descr.version != job.version || descr.status != ProjectStatus::Compiling {
                                cancel = true;
                            }
                        } else {
                            cancel = true;
                        }
                    }
                    if cancel {
                        job.unwrap().cancel().await;
                        job = None;
                    }
                }
                Some(exit_status) = async {
                    if let Some(job) = &mut job {
                        Some(job.wait().await)
                    } else {
                        None
                    }
                }, if job.is_some() => {
                    let project_id = job.as_ref().unwrap().project_id;
                    let version = job.as_ref().unwrap().version;
                    let mut db = db.lock().await;

                    match exit_status {
                        Ok(status) if status.success() && job.as_ref().unwrap().is_sql() => {
                            // SQL compiler succeeded -- start Rust job.
                            job = Some(CompilationJob::rust(&config, project_id, version).await?);
                        }
                        Ok(status) if status.success() && job.as_ref().unwrap().is_rust() => {
                            // Rust compiler succeeded -- declare victory.
                            db.set_project_status_guarded(project_id, version, ProjectStatus::Success).await?;
                            job = None;
                        }
                        Ok(status) => {
                            let output = job.as_ref().unwrap().error_output(&config).await?;
                            let status = if job.as_ref().unwrap().is_rust() {
                                ProjectStatus::RustError(format!("{output}\nexit code: {status}"))
                            } else {
                                ProjectStatus::SqlError(format!("{output}\nexit code: {status}"))
                            };
                            // change project status to error
                            db.set_project_status_guarded(project_id, version, status).await?;
                            job = None;
                        }
                        Err(e) => {
                            let status = if job.unwrap().is_rust() {
                                ProjectStatus::RustError(format!("I/O error: {e}"))
                            } else {
                                ProjectStatus::SqlError(format!("I/O error: {e}"))
                            };
                            // change project status to error
                            db.set_project_status_guarded(project_id, version, status).await?;
                            job = None;
                        }
                    }
                }
            }
            if job.is_none() {
                let mut db = db.lock().await;
                if let Some((project_id, version)) = db.next_job().await? {
                    trace!("next project in the queue: '{project_id}', version '{version}'");
                    job = Some(CompilationJob::sql(&config, &db, project_id, version).await?);
                    db.set_project_status_guarded(project_id, version, ProjectStatus::Compiling)
                        .await?;
                }
            }
        }
    }
}

#[derive(Eq, PartialEq)]
enum Stage {
    Sql,
    Rust,
}

struct CompilationJob {
    stage: Stage,
    project_id: ProjectId,
    version: Version,
    compiler_process: Child,
}

impl CompilationJob {
    fn is_sql(&self) -> bool {
        self.stage == Stage::Sql
    }

    fn is_rust(&self) -> bool {
        self.stage == Stage::Rust
    }

    async fn sql(
        config: &ServerConfig,
        db: &ProjectDB,
        project_id: ProjectId,
        version: Version,
    ) -> AnyResult<Self> {
        debug!("running SQL compiler on project '{project_id}', version '{version}'");

        // Read code from DB (we assume that the DB is locked by the caller,
        // so no need for a version check).
        let (_version, code) = db.project_code(project_id).await?;

        // Create project directory.
        let sql_file_path = config.sql_file_path(project_id);
        let project_directory = sql_file_path.parent().unwrap();
        fs::create_dir_all(&project_directory).await.map_err(|e| {
            AnyError::msg(format!(
                "failed to create project directory '{}': '{e}'",
                project_directory.display()
            ))
        })?;

        // Write SQL code to file.
        fs::write(&sql_file_path, code).await?;

        let rust_file_path = config.rust_program_path(project_id);
        fs::create_dir_all(rust_file_path.parent().unwrap()).await?;

        let stderr_path = config.stderr_path(project_id);
        let err_file = File::create(&stderr_path).await.map_err(|e| {
            AnyError::msg(format!(
                "failed to create error log '{}': '{e}'",
                stderr_path.display()
            ))
        })?;

        let rust_file = File::create(&rust_file_path).await.map_err(|e| {
            AnyError::msg(format!(
                "failed to create '{}': '{e}'",
                rust_file_path.display()
            ))
        })?;

        // Run compiler, direct output to main.rs, direct stderr to file.
        let compiler_process = Command::new(config.sql_compiler_path())
            .arg(sql_file_path.as_os_str())
            .arg("-i")
            .stdin(Stdio::null())
            .stderr(Stdio::from(err_file.into_std().await))
            .stdout(Stdio::from(rust_file.into_std().await))
            .spawn()
            .map_err(|e| {
                AnyError::msg(format!(
                    "failed to start SQL compiler '{}': '{e}'",
                    sql_file_path.display()
                ))
            })?;

        Ok(Self {
            stage: Stage::Sql,
            project_id,
            version,
            compiler_process,
        })
    }

    async fn rust(
        config: &ServerConfig,
        project_id: ProjectId,
        version: Version,
    ) -> AnyResult<Self> {
        debug!("running Rust compiler on project '{project_id}', version '{version}'");

        let mut main_rs = OpenOptions::new()
            .append(true)
            .open(&config.rust_program_path(project_id))
            .await?;
        main_rs.write_all(MAIN_FUNCTION.as_bytes()).await?;
        drop(main_rs);

        // Write `project/Cargo.toml`.
        let template_toml = fs::read_to_string(&config.project_toml_template_path()).await?;
        let project_name = format!("name = \"{}\"", ServerConfig::crate_name(project_id));
        let project_toml_code = template_toml
            .replace("name = \"temp\"", &project_name)
            .replace(", default-features = false", "")
            .replace(
                "[lib]\npath = \"src/lib.rs\"",
                &format!("\n\n[[bin]]\n{project_name}\npath = \"src/main.rs\""),
            );

        fs::write(&config.project_toml_path(project_id), project_toml_code).await?;

        // Write `Cargo.toml`.
        let mut workspace_toml_code = format!(
            "[workspace]\nmembers = [ \"lib/*\", \"{}\"]\n",
            ServerConfig::crate_name(project_id),
        );

        if let Some(dbsp_override_path) = &config.dbsp_override_path {
            let patch = format!(
                "[patch.'https://github.com/vmware/database-stream-processor']\n\
                dbsp = {{ path = \"{dbsp_override_path}\" }}\n\
                dbsp_adapters = {{ path = \"{dbsp_override_path}/adapters\" }}"
            );
            workspace_toml_code.push_str(&patch);
        }

        fs::write(&config.workspace_toml_path(), workspace_toml_code).await?;

        let err_file = File::create(&config.stderr_path(project_id)).await?;
        let out_file = File::create(&config.stdout_path(project_id)).await?;

        // Run cargo, direct stdout and stderr to the same file.
        let mut command = Command::new("cargo");

        command
            .current_dir(&config.workspace_dir())
            .arg("build")
            .arg("--workspace")
            .stdin(Stdio::null())
            .stderr(Stdio::from(err_file.into_std().await))
            .stdout(Stdio::from(out_file.into_std().await));

        if !config.debug {
            command.arg("--release");
        }

        let compiler_process = command.spawn()?;

        Ok(Self {
            stage: Stage::Rust,
            project_id,
            version,
            compiler_process,
        })
    }

    async fn wait(&mut self) -> AnyResult<ExitStatus> {
        let exit_status = self.compiler_process.wait().await?;
        Ok(exit_status)
        // doesn't update status
    }

    async fn error_output(&self, config: &ServerConfig) -> AnyResult<String> {
        let output = match self.stage {
            Stage::Sql => fs::read_to_string(config.stderr_path(self.project_id)).await?,
            Stage::Rust => {
                let stdout = fs::read_to_string(config.stdout_path(self.project_id)).await?;
                let stderr = fs::read_to_string(config.stderr_path(self.project_id)).await?;
                format!("stdout:\n{stdout}\nstderr:\n{stderr}")
            }
        };

        Ok(output)
    }

    async fn cancel(&mut self) {
        let _ = self.compiler_process.kill().await;
    }
}

/*enum CompilerCommand {
    Enqueue(ProjectId, Version),
    Cancel(ProjectId, Version),
}*/

impl Drop for Compiler {
    fn drop(&mut self) {
        self.compiler_task.abort();
    }
}
