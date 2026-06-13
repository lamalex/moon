use crate::PluginType;
use bytes::Bytes;
use extism::{CurrentPlugin, Error, Function, UserData, Val, ValType};
use moon_common::{Id, color};
use moon_config::{
    ExtensionsConfig, ProjectToolchainEntry, ToolchainPluginConfig, ToolchainsConfig,
    WorkspaceConfig,
};
use moon_env::MoonEnvironment;
use moon_target::Target;
use moon_workspace_graph::WorkspaceGraph;
use proto_core::ProtoEnvironment;
use rustc_hash::FxHashMap;
use starbase_console::EmptyReporter;
use starbase_process::{CaptureOptions, ChildExit, Command, Output};
use starbase_utils::json::merge as json_merge;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing::{instrument, trace};
use warpgate::{api::convert_to_real_native_path, host::HostData};

use moon_pdk_api::{
    Id as ProcessCapabilityId, ProcessCapabilityDeclaration, ProcessCommandInput,
    ProcessCommandResult, ProcessOutputChunk, ProcessOutputChunkInput, ProcessOutputCloseInput,
    ProcessOutputCloseOutput, ProcessOutputStream,
};

pub(crate) const VCS_PLUGIN_TIMEOUT_MS: u64 = 120_000;
const PROCESS_COMMAND_TIMEOUT: Duration = Duration::from_millis(VCS_PLUGIN_TIMEOUT_MS);
const PROCESS_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const PROCESS_OUTPUT_CHUNK_SIZE: u64 = 64 * 1024;
const PROCESS_OUTPUT_LIMIT: usize = 256 * 1024 * 1024;
const PROCESS_RESPONSE_RESERVE: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub struct MoonHostData {
    pub moon_env: Arc<MoonEnvironment>,
    pub proto_env: Arc<ProtoEnvironment>,
    pub extensions_config: Arc<ExtensionsConfig>,
    pub toolchains_config: Arc<ToolchainsConfig>,
    pub workspace_config: Arc<WorkspaceConfig>,
    pub workspace_graph: Arc<OnceLock<Arc<WorkspaceGraph>>>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessHostAccess {
    executables: Arc<Mutex<Option<FxHashMap<ProcessCapabilityId, PathBuf>>>>,
}

impl ProcessHostAccess {
    pub fn configure(
        &self,
        capabilities: &[ProcessCapabilityDeclaration],
        workspace_root: &Path,
    ) -> miette::Result<()> {
        let mut executables = self
            .executables
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        if executables.is_some() {
            return Err(miette::miette!(
                "plugin process capabilities are already configured"
            ));
        }

        let mut resolved = FxHashMap::default();

        for capability in capabilities {
            if capability.executable.is_empty()
                || Path::new(&capability.executable).components().count() != 1
            {
                return Err(miette::miette!(
                    "process capability `{}` must declare an executable name without a path",
                    capability.id
                ));
            }

            let executable = resolve_process_executable(&capability.executable, workspace_root)
                .ok_or_else(|| {
                    miette::miette!(
                        "unable to resolve executable `{}` for process capability `{}`",
                        capability.executable,
                        capability.id
                    )
                })?;

            if resolved.insert(capability.id.clone(), executable).is_some() {
                return Err(miette::miette!(
                    "process capability `{}` is declared more than once",
                    capability.id
                ));
            }
        }

        *executables = Some(resolved);

        Ok(())
    }

    fn executable(&self, capability: &ProcessCapabilityId) -> Result<PathBuf, Error> {
        self.executables
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .ok_or_else(|| Error::msg("plugin process capabilities are not configured"))?
            .get(capability)
            .cloned()
            .ok_or_else(|| {
                Error::msg(format!(
                    "plugin requested undeclared process capability `{capability}`"
                ))
            })
    }
}

impl fmt::Debug for MoonHostData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoonHostData")
            .field("moon_env", &self.moon_env)
            .field("proto_env", &self.proto_env)
            .field("extensions_config", &self.extensions_config)
            .field("toolchains_config", &self.toolchains_config)
            .field("workspace_config", &self.workspace_config)
            .finish()
    }
}

#[derive(Clone)]
struct ProcessHostData {
    access: ProcessHostAccess,
    output: Arc<Mutex<ProcessOutputState>>,
    shared: HostData,
    workspace_root: PathBuf,
}

#[derive(Default)]
struct ProcessOutputState {
    next_id: u64,
    output: Option<ProcessOutput>,
}

struct ProcessOutput {
    id: u64,
    stderr: Bytes,
    stdout: Bytes,
}

impl ProcessOutput {
    fn read(&self, stream: ProcessOutputStream, offset: u64) -> Result<ProcessOutputChunk, Error> {
        let output = match stream {
            ProcessOutputStream::Stderr => &self.stderr,
            ProcessOutputStream::Stdout => &self.stdout,
        };
        let len = output.len() as u64;

        if offset > len {
            return Err(Error::msg(format!(
                "process output offset {offset} exceeds stream length {len}"
            )));
        }

        let end = offset.saturating_add(PROCESS_OUTPUT_CHUNK_SIZE).min(len);
        let bytes = &output[offset as usize..end as usize];

        Ok(ProcessOutputChunk::from_bytes(bytes, end >= len))
    }
}

pub(crate) fn create_host_functions(
    plugin_type: PluginType,
    data: MoonHostData,
    shared_data: HostData,
    process_access: Option<ProcessHostAccess>,
) -> Vec<Function> {
    let mut functions = warpgate::host::create_host_functions(shared_data.clone());

    if matches!(plugin_type, PluginType::Vcs) {
        functions.retain(|function| function.name() == "host_log");
        let process_data = ProcessHostData {
            access: process_access.expect("VCS plugins require process host access"),
            output: Arc::new(Mutex::new(ProcessOutputState::default())),
            shared: shared_data,
            workspace_root: data.moon_env.workspace_root.clone(),
        };
        functions.push(Function::new(
            "exec_process_command_v1",
            [ValType::I64],
            [ValType::I64],
            UserData::new(process_data.clone()),
            exec_process_command_v1,
        ));
        functions.push(Function::new(
            "read_process_output_v1",
            [ValType::I64],
            [ValType::I64],
            UserData::new(process_data.clone()),
            read_process_output_v1,
        ));
        functions.push(Function::new(
            "close_process_output_v1",
            [ValType::I64],
            [ValType::I64],
            UserData::new(process_data),
            close_process_output_v1,
        ));

        return functions;
    }

    functions.extend(vec![
        Function::new(
            "load_extension_config_by_id",
            [ValType::I64],
            [ValType::I64],
            UserData::new(data.clone()),
            load_extension_config_by_id,
        ),
        Function::new(
            "load_project_by_id",
            [ValType::I64],
            [ValType::I64],
            UserData::new(data.clone()),
            load_project,
        ),
        Function::new(
            "load_projects_by_id",
            [ValType::I64],
            [ValType::I64],
            UserData::new(data.clone()),
            load_projects,
        ),
        Function::new(
            "load_task_by_target",
            [ValType::I64],
            [ValType::I64],
            UserData::new(data.clone()),
            load_task,
        ),
        Function::new(
            "load_tasks_by_target",
            [ValType::I64],
            [ValType::I64],
            UserData::new(data.clone()),
            load_tasks,
        ),
        Function::new(
            "load_toolchain_config_by_id",
            [ValType::I64, ValType::I64],
            [ValType::I64],
            UserData::new(data),
            load_toolchain_config_by_id,
        ),
    ]);
    functions
}

fn resolve_process_executable(executable: &str, workspace_root: &Path) -> Option<PathBuf> {
    let workspace_root = workspace_root.canonicalize().ok()?;
    let path = std::env::var_os("PATH")?;

    for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
        let candidates = if std::env::consts::EXE_SUFFIX.is_empty() {
            vec![directory.join(executable)]
        } else {
            vec![
                directory.join(executable),
                directory.join(format!("{executable}{}", std::env::consts::EXE_SUFFIX)),
            ]
        };

        for candidate in candidates {
            if candidate.starts_with(&workspace_root) || !candidate.is_file() {
                continue;
            }

            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };

            if !candidate.starts_with(&workspace_root) {
                return Some(candidate);
            }
        }
    }

    None
}

fn exec_process_command_v1(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<ProcessHostData>,
) -> Result<(), Error> {
    let input: ProcessCommandInput = serde_json::from_str(plugin.memory_get_val(&inputs[0])?)?;
    let data = user_data.get()?;
    let data = data.lock().unwrap_or_else(|error| error.into_inner());
    let result_id = {
        let mut state = data
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::msg("process result IDs are exhausted"))?;
        state.output = None;
        state.next_id
    };
    let cwd = input
        .cwd
        .as_ref()
        .map(|path| convert_to_real_native_path(path, &data.shared.virtual_paths))
        .unwrap_or_else(|| data.shared.working_dir.clone());
    let workspace_root = data.workspace_root.canonicalize()?;
    let cwd = cwd.canonicalize()?;

    validate_process_cwd(&cwd, &workspace_root)?;

    let timeout = plugin
        .time_remaining()
        .map(|remaining| remaining.saturating_sub(PROCESS_RESPONSE_RESERVE))
        .map_or(PROCESS_COMMAND_TIMEOUT, |remaining| {
            remaining.min(PROCESS_COMMAND_TIMEOUT)
        });
    let result = execute_process_command(
        data.access.executable(&input.capability)?,
        &cwd,
        &input,
        timeout,
    )?;
    let output = ProcessCommandResult {
        result_id,
        exit_code: match &result.exit {
            ChildExit::Completed(status) => status.code().unwrap_or(-1),
            _ => -1,
        },
        stdout_len: result.stdout.len() as u64,
        stderr_len: result.stderr.len() as u64,
    };
    let process_output = ProcessOutput {
        id: result_id,
        stderr: result.stderr,
        stdout: result.stdout,
    };
    data.output
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .output = Some(process_output);

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&output)?)?;

    Ok(())
}

fn process_capture_options(timeout: Duration) -> CaptureOptions {
    CaptureOptions {
        timeout: Some(timeout),
        output_limit: Some(PROCESS_OUTPUT_LIMIT),
        output_drain_timeout: Some(PROCESS_OUTPUT_DRAIN_TIMEOUT),
    }
}

fn execute_process_command(
    executable: PathBuf,
    cwd: &Path,
    input: &ProcessCommandInput,
    timeout: Duration,
) -> Result<Output, Error> {
    let mut command = Command::<EmptyReporter>::new(executable);
    command
        .cwd(cwd)
        .args(&input.args)
        .envs(&input.env)
        .no_shell()
        .set_error_on_nonzero(false);

    command
        .exec_capture_output_to_memory_blocking(&process_capture_options(timeout))
        .map_err(map_error)
}

fn read_process_output_v1(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<ProcessHostData>,
) -> Result<(), Error> {
    let input: ProcessOutputChunkInput = serde_json::from_str(plugin.memory_get_val(&inputs[0])?)?;
    let data = user_data.get()?;
    let data = data.lock().unwrap_or_else(|error| error.into_inner());
    let state = data
        .output
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let output = state
        .output
        .as_ref()
        .filter(|output| output.id == input.result_id)
        .ok_or_else(|| Error::msg(format!("unknown process result `{}`", input.result_id)))?;
    let chunk = output.read(input.stream, input.offset)?;

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&chunk)?)?;

    Ok(())
}

fn close_process_output_v1(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<ProcessHostData>,
) -> Result<(), Error> {
    let input: ProcessOutputCloseInput = serde_json::from_str(plugin.memory_get_val(&inputs[0])?)?;
    let data = user_data.get()?;
    let data = data.lock().unwrap_or_else(|error| error.into_inner());
    let mut state = data
        .output
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    if !state
        .output
        .as_ref()
        .is_some_and(|output| output.id == input.result_id)
    {
        return Err(Error::msg(format!(
            "unknown process result `{}`",
            input.result_id
        )));
    }

    state.output = None;
    plugin.memory_set_val(
        &mut outputs[0],
        serde_json::to_string(&ProcessOutputCloseOutput {})?,
    )?;

    Ok(())
}

fn validate_process_cwd(cwd: &Path, workspace_root: &Path) -> Result<(), Error> {
    if cwd.starts_with(workspace_root) {
        Ok(())
    } else {
        Err(Error::msg(
            "plugin process working directory must be inside the workspace",
        ))
    }
}

fn map_error(error: miette::Report) -> Error {
    Error::msg(error.to_string())
}

#[instrument(name = "host_load_project_by_id", skip_all)]
fn load_project(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let id_raw: String = plugin.memory_get_val(&inputs[0])?;
    let id = Id::new(id_raw)?;
    let uuid = plugin.id().to_string();

    trace!(
        plugin = &uuid,
        project_id = id.as_str(),
        "Calling host function {}",
        color::label("load_project_by_id"),
    );

    let data = user_data.get()?;
    let data = data.lock().unwrap();
    let project = data
        .workspace_graph
        .get()
        .unwrap()
        .get_project(&id)
        .map_err(map_error)?;

    trace!(
        plugin = &uuid,
        project_id = id.as_str(),
        "Called host function {}",
        color::label("load_project_by_id"),
    );

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&project)?)?;

    Ok(())
}

#[instrument(name = "host_load_projects_by_id", skip_all)]
fn load_projects(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let ids_raw: String = plugin.memory_get_val(&inputs[0])?;
    let ids: Vec<String> = serde_json::from_str(&ids_raw)?;
    let uuid = plugin.id().to_string();

    trace!(
        plugin = &uuid,
        project_ids = ?ids,
        "Calling host function {}",
        color::label("load_projects_by_id"),
    );

    let data = user_data.get()?;
    let data = data.lock().unwrap();
    let workspace_graph = data.workspace_graph.get().unwrap();
    let mut projects = FxHashMap::default();

    for id in &ids {
        let id = Id::raw(id);
        let project = workspace_graph.get_project(&id).map_err(map_error)?;

        projects.insert(id, project);
    }

    trace!(
        plugin = &uuid,
        project_ids = ?ids,
        "Called host function {}",
        color::label("load_projects_by_id"),
    );

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&projects)?)?;

    Ok(())
}

#[instrument(name = "host_load_task_by_target", skip_all)]
fn load_task(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let target_raw: String = plugin.memory_get_val(&inputs[0])?;
    let target = Target::parse(&target_raw).map_err(map_error)?;
    let uuid = plugin.id().to_string();

    trace!(
        plugin = &uuid,
        task_target = target.as_str(),
        "Calling host function {}",
        color::label("load_task_by_target"),
    );

    if target.get_project_id().is_err() {
        return Err(Error::msg(format!(
            "Unable to load task {target}. Requires a fully-qualified target with a project scope."
        )));
    };

    let data = user_data.get()?;
    let data = data.lock().unwrap();
    let task = data
        .workspace_graph
        .get()
        .unwrap()
        .get_task(&target)
        .map_err(map_error)?;

    trace!(
        plugin = &uuid,
        task_target = target.as_str(),
        "Called host function {}",
        color::label("load_task_by_target"),
    );

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&task)?)?;

    Ok(())
}

#[instrument(name = "host_load_tasks_by_target", skip_all)]
fn load_tasks(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let targets_raw: String = plugin.memory_get_val(&inputs[0])?;
    let targets: Vec<String> = serde_json::from_str(&targets_raw)?;
    let uuid = plugin.id().to_string();

    trace!(
        plugin = &uuid,
        task_targets = ?targets,
        "Calling host function {}",
        color::label("load_tasks_by_target"),
    );

    let data = user_data.get()?;
    let data = data.lock().unwrap();
    let workspace_graph = data.workspace_graph.get().unwrap();
    let mut tasks = FxHashMap::default();

    for target in &targets {
        let target = Target::parse(target).map_err(map_error)?;

        if target.get_project_id().is_err() {
            return Err(Error::msg(format!(
                "Unable to load task {target}. Requires a fully-qualified target with a project scope."
            )));
        };

        let task = workspace_graph.get_task(&target).map_err(map_error)?;

        tasks.insert(target, task);
    }

    trace!(
        plugin = &uuid,
        task_targets = ?targets,
        "Called host function {}",
        color::label("load_tasks_by_target"),
    );

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&tasks)?)?;

    Ok(())
}

#[instrument(name = "host_load_extension_config_by_id", skip_all)]
fn load_extension_config_by_id(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let uuid = plugin.id().to_string();
    let extension_id = Id::new(plugin.memory_get_val::<String>(&inputs[0])?)?;

    trace!(
        plugin = &uuid,
        extension_id = extension_id.as_str(),
        "Calling host function {}",
        color::label("load_extension_config_by_id"),
    );

    let data = user_data.get()?;
    let data = data.lock().unwrap();

    let config = data
        .extensions_config
        .get_plugin_config(&extension_id)
        .ok_or_else(|| {
            Error::msg(format!(
                "Unable to load extension configuration. Extension {extension_id} does not exist."
            ))
        })?;

    plugin.memory_set_val(&mut outputs[0], serde_json::to_string(&config.to_json())?)?;

    trace!(
        plugin = &uuid,
        extension_id = extension_id.as_str(),
        "Called host function {}",
        color::label("load_extension_config_by_id"),
    );

    Ok(())
}

#[instrument(name = "host_load_toolchain_config_by_id", skip_all)]
fn load_toolchain_config_by_id(
    plugin: &mut CurrentPlugin,
    inputs: &[Val],
    outputs: &mut [Val],
    user_data: UserData<MoonHostData>,
) -> Result<(), Error> {
    let uuid = plugin.id().to_string();
    let toolchain_id = Id::new(plugin.memory_get_val::<String>(&inputs[0])?)?;
    let mut project_id = None;

    if let Some(input) = inputs.get(1) {
        let id = plugin.memory_get_val::<String>(input)?;

        // Extism passes it through as empty
        if !id.is_empty() {
            project_id.replace(Id::new(id)?);
        }
    }

    trace!(
        plugin = &uuid,
        project_id = project_id.as_ref().map(|id| id.as_str()),
        toolchain_id = toolchain_id.as_str(),
        "Calling host function {}",
        color::label("load_toolchain_config_by_id"),
    );

    let data = user_data.get()?;
    let data = data.lock().unwrap();

    let default_config = ToolchainPluginConfig::default();
    let root_config = data
        .toolchains_config
        .get_plugin_config(&toolchain_id)
        .ok_or_else(|| {
            Error::msg(format!(
                "Unable to load toolchain configuration. Toolchain {toolchain_id} does not exist."
            ))
        })?;

    match &project_id {
        Some(project_id) => {
            let workspace_graph = data.workspace_graph.get().unwrap();
            let project = workspace_graph.get_project(project_id).map_err(map_error)?;

            let config = project
                .config
                .toolchains
                .get_plugin_config(&toolchain_id)
                .and_then(|entry| match entry {
                    ProjectToolchainEntry::Object(cfg) => Some(cfg),
                    _ => None,
                })
                .unwrap_or(&default_config);

            // We don't have access to the toolchain registry here,
            // so we must manually merge these config objects
            plugin.memory_set_val(
                &mut outputs[0],
                serde_json::to_string(&json_merge(&root_config.to_json(), &config.to_json()))?,
            )?;
        }
        None => {
            plugin.memory_set_val(
                &mut outputs[0],
                serde_json::to_string(&root_config.to_json())?,
            )?;
        }
    };

    trace!(
        plugin = &uuid,
        project_id = project_id.as_ref().map(|id| id.as_str()),
        toolchain_id = toolchain_id.as_str(),
        "Called host function {}",
        color::label("load_toolchain_config_by_id"),
    );

    Ok(())
}

#[cfg(test)]
mod process_host_tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::Command as StdCommand;
    #[cfg(unix)]
    use std::time::Instant;

    #[cfg(unix)]
    fn create_executable(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    struct KillPidOnDrop {
        identity: String,
        pid_file: Option<PathBuf>,
    }

    #[cfg(unix)]
    impl KillPidOnDrop {
        fn new(path: PathBuf, identity: &Path) -> Self {
            Self {
                identity: identity.to_string_lossy().into_owned(),
                pid_file: Some(path),
            }
        }

        fn is_running(&self) -> bool {
            let Some(path) = &self.pid_file else {
                return false;
            };
            let Ok(pid) = std::fs::read_to_string(path) else {
                return false;
            };

            StdCommand::new("ps")
                .args(["-p", pid.trim(), "-o", "command="])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains(&self.identity)
                })
        }

        fn kill(&mut self) {
            let Some(path) = self.pid_file.take() else {
                return;
            };
            let Ok(pid) = std::fs::read_to_string(path) else {
                return;
            };

            if !StdCommand::new("ps")
                .args(["-p", pid.trim(), "-o", "command="])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains(&self.identity)
                })
            {
                return;
            }

            let _ = StdCommand::new("kill").args(["-KILL", pid.trim()]).status();
            let deadline = Instant::now() + Duration::from_secs(2);

            while StdCommand::new("kill")
                .args(["-0", pid.trim()])
                .status()
                .is_ok_and(|status| status.success())
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn disarm(&mut self) {
            self.pid_file = None;
        }
    }

    #[cfg(unix)]
    impl Drop for KillPidOnDrop {
        fn drop(&mut self) {
            self.kill();
        }
    }

    #[cfg(unix)]
    fn process_input(args: &[&str]) -> ProcessCommandInput {
        ProcessCommandInput {
            capability: ProcessCapabilityId::raw("test"),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: None,
            env: Default::default(),
        }
    }

    #[test]
    fn bounds_process_execution_output_and_drain() {
        let options = process_capture_options(Duration::from_secs(10));

        assert_eq!(options.timeout, Some(Duration::from_secs(10)));
        assert_eq!(options.output_limit, Some(PROCESS_OUTPUT_LIMIT));
        assert_eq!(
            options.output_drain_timeout,
            Some(PROCESS_OUTPUT_DRAIN_TIMEOUT)
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounds_output_drain_when_a_descendant_holds_the_pipes() {
        let workspace = starbase_sandbox::create_empty_sandbox();
        let helper = workspace.path().join("pipe-holder.sh");
        let descendant = workspace.path().join("pipe-descendant.sh");
        let child_pid = workspace.path().join("pipe-holder.pid");
        let mut child = KillPidOnDrop::new(child_pid.clone(), &descendant);
        create_executable(&descendant, "#!/bin/sh\nsleep 30\nexit 0\n");
        create_executable(
            &helper,
            r#"#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$dir/pipe-descendant.sh" &
printf '%s\n' "$!" > "$dir/pipe-holder.pid"
printf 'direct child completed\n'
"#,
        );
        let started = Instant::now();

        let error = execute_process_command(
            helper,
            workspace.path(),
            &process_input(&[]),
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("output did not drain"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(child_pid.exists());
        assert!(child.is_running());
        child.kill();
    }

    #[cfg(unix)]
    #[test]
    fn times_out_and_terminates_a_commit_signing_helper() {
        let workspace = starbase_sandbox::create_empty_sandbox();
        let signer = workspace.path().join("signer.sh");
        let signer_pid = workspace.path().join("signer.pid");
        let mut signer_process = KillPidOnDrop::new(signer_pid.clone(), &signer);
        create_executable(
            &signer,
            r#"#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$$" > "$dir/signer.pid"
sleep 30
exit 1
"#,
        );
        workspace.run_git(|command| {
            command.args(["init", "--initial-branch", "master"]);
        });
        workspace.run_git(|command| {
            command.args(["config", "user.name", "Moon"]);
        });
        workspace.run_git(|command| {
            command.args(["config", "user.email", "moon@example.com"]);
        });
        workspace.run_git(|command| {
            command.args(["config", "commit.gpgSign", "true"]);
        });
        workspace.run_git(|command| {
            command.args(["config", "gpg.format", "openpgp"]);
        });
        workspace.run_git(|command| {
            command.args([
                "config",
                "gpg.program",
                signer.to_str().expect("signer path must be UTF-8"),
            ]);
        });
        workspace.create_file("signed.txt", "signed");
        workspace.run_git(|command| {
            command.args(["add", "signed.txt"]);
        });
        let git = resolve_process_executable("git", workspace.path()).unwrap();
        let started = Instant::now();

        let error = execute_process_command(
            git,
            workspace.path(),
            &process_input(&["commit", "-m", "signed"]),
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("exceeded its 5s timeout"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(signer_pid.exists(), "Git did not invoke the signing helper");

        let deadline = Instant::now() + Duration::from_secs(2);
        while signer_process.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !signer_process.is_running(),
            "signing helper survived cleanup"
        );
        signer_process.disarm();
    }

    #[test]
    fn resolves_declared_executables_without_a_core_allowlist() {
        let workspace = starbase_sandbox::create_empty_sandbox();
        let access = ProcessHostAccess::default();
        access
            .configure(
                &[ProcessCapabilityDeclaration {
                    id: ProcessCapabilityId::raw("vcs"),
                    executable: "git".into(),
                }],
                workspace.path(),
            )
            .unwrap();

        assert!(access.executable(&ProcessCapabilityId::raw("vcs")).is_ok());
        assert!(
            access
                .executable(&ProcessCapabilityId::raw("undeclared"))
                .is_err()
        );
    }

    #[test]
    fn rejects_executable_paths_in_capability_declarations() {
        let workspace = starbase_sandbox::create_empty_sandbox();
        let access = ProcessHostAccess::default();

        assert!(
            access
                .configure(
                    &[ProcessCapabilityDeclaration {
                        id: ProcessCapabilityId::raw("vcs"),
                        executable: "../vcs".into(),
                    }],
                    workspace.path(),
                )
                .is_err()
        );
    }

    #[test]
    fn confines_process_working_directories_to_the_workspace() {
        let workspace = starbase_sandbox::create_empty_sandbox();
        let outside = starbase_sandbox::create_empty_sandbox();
        let nested = workspace.path().join("nested");
        std::fs::create_dir(&nested).unwrap();

        assert!(validate_process_cwd(workspace.path(), workspace.path()).is_ok());
        assert!(validate_process_cwd(&nested, workspace.path()).is_ok());
        assert!(validate_process_cwd(outside.path(), workspace.path()).is_err());
    }

    #[test]
    fn reads_chunked_output() {
        let bytes = vec![b'x'; PROCESS_OUTPUT_CHUNK_SIZE as usize + 3];
        let output = ProcessOutput {
            id: 1,
            stderr: Bytes::new(),
            stdout: Bytes::from(bytes),
        };

        let first = output.read(ProcessOutputStream::Stdout, 0).unwrap();
        assert_eq!(
            first.decode().unwrap().len(),
            PROCESS_OUTPUT_CHUNK_SIZE as usize
        );
        assert!(!first.eof);

        let second = output
            .read(ProcessOutputStream::Stdout, PROCESS_OUTPUT_CHUNK_SIZE)
            .unwrap();
        assert_eq!(second.decode().unwrap(), b"xxx");
        assert!(second.eof);
    }
}
