use crate::{
    NewPipelineRequest, NewPipelineResponse, PipelineId, ProjectDB, ProjectId, ProjectStatus,
    ServerConfig, Version,
};
use actix_web::HttpResponse;
use anyhow::{Error as AnyError, Result as AnyResult};
use regex::Regex;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
};
use tokio::{
    fs,
    fs::{create_dir_all, File},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeek, BufReader, SeekFrom},
    process::{Child, Command},
    sync::Mutex,
    time::{sleep, Duration, Instant},
};

const STARTUP_TIMEOUT: Duration = Duration::from_millis(10_000);

pub struct RunnerConfig {
    pub pipeline_directory: String,
}

impl RunnerConfig {
    fn pipeline_dir(&self, pipeline_id: PipelineId) -> PathBuf {
        Path::new(&self.pipeline_directory).join(format!("pipeline{pipeline_id}"))
    }

    fn config_file_path(&self, pipeline_id: PipelineId) -> PathBuf {
        self.pipeline_dir(pipeline_id).join("config.yaml")
    }

    fn metadata_file_path(&self, pipeline_id: PipelineId) -> PathBuf {
        self.pipeline_dir(pipeline_id).join("metadata.json")
    }

    fn log_file_path(&self, pipeline_id: PipelineId) -> PathBuf {
        self.pipeline_dir(pipeline_id).join("pipeline.log")
    }
}

#[derive(Serialize)]
struct PipelineMetadata {
    project_id: ProjectId,
    version: Version,
    code: String,
}

pub(crate) async fn run_pipeline(
    config: &ServerConfig,
    dblock: &Mutex<ProjectDB>,
    request: &NewPipelineRequest,
) -> AnyResult<HttpResponse> {
    let db = dblock.lock().await;

    // Check: project exists, version = current version, compilation completed.
    match db.project_status(request.project_id).await? {
        None => {
            return Ok(HttpResponse::BadRequest()
                .body(format!("unknown project id '{}'", request.project_id)));
        }
        Some((version, _status)) if version != request.version => {
            return Ok(HttpResponse::Conflict().body(format!(
                "specified version '{}' does not match the latest project version '{}'",
                request.version, version
            )));
        }
        Some((_version, status)) if status != ProjectStatus::Success => {
            return Ok(HttpResponse::Conflict().body(format!("project hasn't been compiled yet")));
        }
        _ => {}
    }

    let pipeline_id = db.alloc_pipeline_id().await?;

    let mut pipeline_process = start(config, &db, request, pipeline_id).await?;

    // Unlock db -- the next part can be slow.
    drop(db);

    // Start listening to log file until either port number or error shows up or
    // child process exits.
    match wait_for_startup(&config.runner_config.log_file_path(pipeline_id)).await {
        Ok(port) => {
            // Store pipeline in the database.
            if let Err(e) = dblock
                .lock()
                .await
                .new_pipeline(pipeline_id, request.project_id, request.version, port)
                .await
            {
                let _ = pipeline_process.kill().await;
                return Err(e);
            };
            let json_string =
                serde_json::to_string(&NewPipelineResponse { pipeline_id, port }).unwrap();

            Ok(HttpResponse::Ok()
                .content_type(mime::APPLICATION_JSON)
                .body(json_string))
        }
        Err(e) => {
            let _ = pipeline_process.kill().await;
            Err(e)
        }
    }
}

async fn start(
    config: &ServerConfig,
    db: &ProjectDB,
    request: &NewPipelineRequest,
    pipeline_id: PipelineId,
) -> AnyResult<Child> {
    // Create pipeline directory (delete old directory if exists); write metadata
    // and config files to it.
    let pipeline_dir = config.runner_config.pipeline_dir(pipeline_id);
    create_dir_all(&pipeline_dir).await?;

    let config_file_path = config.runner_config.config_file_path(pipeline_id);
    fs::write(&config_file_path, &request.config_yaml).await?;

    let (_version, code) = db.project_code(request.project_id).await?;

    let metadata = PipelineMetadata {
        project_id: request.project_id,
        version: request.version,
        code,
    };
    let metadata_file_path = config.runner_config.metadata_file_path(pipeline_id);
    fs::write(
        &metadata_file_path,
        serde_json::to_string(&metadata).unwrap(),
    )
    .await?;

    let log_file_path = config.runner_config.log_file_path(pipeline_id);
    let log_file = File::create(&log_file_path).await?;
    let out_file = log_file.try_clone().await?;

    // Locate project executable.
    let executable = config
        .compiler_config
        .project_executable(request.project_id);

    // Run executable, set current directory to pipeline directory, pass metadata
    // file and config as arguments.
    let pipeline_process = Command::new(&executable)
        .arg("--config-file")
        .arg(&config_file_path)
        .arg("--metadata-file")
        .arg(&metadata_file_path)
        .stdin(Stdio::null())
        .stdout(out_file.into_std().await)
        .stderr(log_file.into_std().await)
        .spawn()
        .map_err(|e| AnyError::msg(format!("failed to run '{}': {e}", executable.display())))?;

    Ok(pipeline_process)
}

async fn wait_for_startup(log_file_path: &Path) -> AnyResult<u16> {
    let mut log_file_lines = BufReader::new(File::open(log_file_path).await?).lines();

    let start = Instant::now();

    let portnum_regex = Regex::new(r"Started HTTP server on port (\w+)\b").unwrap();
    let error_regex = Regex::new(r"Failed to create server.*").unwrap();

    loop {
        if let Some(line) = log_file_lines.next_line().await? {
            if let Some(captures) = portnum_regex.captures(&line) {
                if let Some(portnum_match) = captures.get(1) {
                    if let Ok(port) = portnum_match.as_str().parse::<u16>() {
                        return Ok(port);
                    } else {
                        return Err(AnyError::msg("invalid port number in log: '{line}'"));
                    }
                } else {
                    return Err(AnyError::msg(
                        "couldn't parse server port number from log: '{line}'",
                    ));
                }
            };
            if let Some(mtch) = error_regex.find(&line) {
                return Err(AnyError::msg(mtch.as_str().to_string()));
            };
        }

        if start.elapsed() > STARTUP_TIMEOUT {
            let log = log_suffix(log_file_path).await;
            return Err(AnyError::msg(format!("waiting for pipeline initialization status timed out after {STARTUP_TIMEOUT:?}\n{log}")));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn log_suffix_inner(log_file_path: &Path) -> AnyResult<String> {
    let mut buf = Vec::with_capacity(10000);

    let mut file = File::open(log_file_path).await?;

    Pin::new(&mut file).start_seek(SeekFrom::End(-10000))?;
    file.read_to_end(&mut buf).await?;

    let suffix = String::from_utf8_lossy(&buf);
    Ok(format!("log file tail:\n{suffix}"))
}

async fn log_suffix(log_file_path: &Path) -> String {
    log_suffix_inner(log_file_path)
        .await
        .unwrap_or_else(|e| format!("[unable to read log file: {e}]"))
}

/*
fn pipeline_status(pipeline_id) -> PipelineStatus {
    // Check that there is a server on the port and its metadata matches pipeline description.
}
*/