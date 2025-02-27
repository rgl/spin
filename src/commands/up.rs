mod app_source;

use std::{
    ffi::OsString,
    fmt::Debug,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser};
use itertools::Itertools;
use reqwest::Url;
use spin_app::locked::LockedApp;
use spin_common::ui::quoted_path;
use spin_loader::FilesMountStrategy;
use spin_oci::OciLoader;
use spin_trigger::cli::{SPIN_LOCAL_APP_DIR, SPIN_LOCKED_URL, SPIN_WORKING_DIR};
use tempfile::TempDir;

use futures::StreamExt;

use crate::opts::*;

use self::app_source::{AppSource, ResolvedAppSource};

const APPLICATION_OPT: &str = "APPLICATION";

/// Start the Fermyon runtime.
#[derive(Parser, Debug, Default)]
#[clap(
    about = "Start the Spin application",
    allow_hyphen_values = true,
    disable_help_flag = true
)]
pub struct UpCommand {
    #[clap(short = 'h', long = "help")]
    pub help: bool,

    /// The application to run. This may be a manifest (spin.toml) file, a
    /// directory containing a spin.toml file, or a remote registry reference.
    /// If omitted, it defaults to "spin.toml".
    #[clap(
        name = APPLICATION_OPT,
        short = 'f',
        long = "from",
        group = "source",
    )]
    pub app_source: Option<String>,

    /// The application to run. This is the same as `--from` but forces the
    /// application to be interpreted as a file or directory path.
    #[clap(
        hide = true,
        name = APP_MANIFEST_FILE_OPT,
        long = "from-file",
        alias = "file",
        group = "source",
    )]
    pub file_source: Option<PathBuf>,

    /// The application to run. This is the same as `--from` but forces the
    /// application to be interpreted as an OCI registry reference.
    #[clap(
        hide = true,
        name = FROM_REGISTRY_OPT,
        long = "from-registry",
        group = "source",
    )]
    pub registry_source: Option<String>,

    /// Ignore server certificate errors from a registry
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Pass an environment variable (key=value) to all components of the application.
    #[clap(short = 'e', long = "env", parse(try_from_str = parse_env_var))]
    pub env: Vec<(String, String)>,

    /// Temporary directory for the static assets of the components.
    #[clap(long = "temp")]
    pub tmp: Option<PathBuf>,

    /// For local apps with directory mounts and no excluded files, mount them directly instead of using a temporary
    /// directory.
    ///
    /// This allows you to update the assets on the host filesystem such that the updates are visible to the guest
    /// without a restart.  This cannot be used with registry apps or apps which use file patterns and/or exclusions.
    #[clap(long, takes_value = false)]
    pub direct_mounts: bool,

    /// For local apps, specifies to perform `spin build` before running the application.
    ///
    /// This is ignored on remote applications, as they are already built.
    #[clap(long, takes_value = false, env = ALWAYS_BUILD_ENV)]
    pub build: bool,

    /// All other args, to be passed through to the trigger
    #[clap(hide = true)]
    pub trigger_args: Vec<OsString>,
}

impl UpCommand {
    pub async fn run(self) -> Result<()> {
        // For displaying help, first print `spin up`'s own usage text, then
        // attempt to load an app and print trigger-type-specific usage.
        let help = self.help;
        if help {
            Self::command()
                .name("spin-up")
                .bin_name("spin up")
                .print_help()?;
            println!();
        }
        self.run_inner().await.or_else(|err| {
            if help {
                tracing::warn!("Error resolving trigger-specific help: {err:?}");
                Ok(())
            } else {
                Err(err)
            }
        })
    }

    async fn run_inner(self) -> Result<()> {
        let app_source = self.app_source();

        if app_source == AppSource::None {
            if self.help {
                let mut child = self
                    .start_trigger(trigger_command(HELP_ARGS_ONLY_TRIGGER_TYPE), None)
                    .await?;
                let _ = child.wait().await?;
                return Ok(());
            } else {
                bail!("Default file '{DEFAULT_MANIFEST_FILE}' not found. Run `spin up --from <APPLICATION>`, or `spin up --help` for usage.");
            }
        }

        if self.build {
            app_source.build().await?;
        }

        // Get working dir holder and hold on to it for the rest of the function.
        // If the working dir is a temporary dir it will be deleted on drop.
        let working_dir_holder = self.get_canonical_working_dir()?;
        let working_dir = working_dir_holder
            .path()
            .canonicalize()
            .context("Could not canonicalize working directory")?;

        let resolved_app_source = self.resolve_app_source(&app_source, &working_dir).await?;

        let trigger_cmds = trigger_command_for_resolved_app_source(&resolved_app_source)
            .with_context(|| format!("Couldn't find trigger executor for {app_source}"))?;

        if self.help {
            for cmd in trigger_cmds {
                let mut help_process = self.start_trigger(cmd.clone(), None).await?;
                _ = help_process.wait().await;
            }
            return Ok(());
        }

        let mut locked_app = self
            .load_resolved_app_source(resolved_app_source, &working_dir)
            .await?;

        self.update_locked_app(&mut locked_app);
        let locked_url = self.write_locked_app(&locked_app, &working_dir).await?;

        let local_app_dir = app_source.local_app_dir().map(Into::into);

        let run_opts = RunTriggerOpts {
            locked_url,
            working_dir,
            local_app_dir,
        };

        let mut trigger_processes = self.start_trigger_processes(trigger_cmds, run_opts).await?;

        set_kill_on_ctrl_c(&trigger_processes)?;

        let mut trigger_tasks = trigger_processes
            .iter_mut()
            .map(|ch| ch.wait())
            .collect::<futures::stream::FuturesUnordered<_>>();

        let first_to_finish = trigger_tasks.next().await;

        if let Some(process_result) = first_to_finish {
            let status = process_result?;
            if !status.success() {
                return Err(crate::subprocess::ExitStatusError::new(status).into());
            }
        }

        Ok(())
    }

    fn get_canonical_working_dir(&self) -> Result<WorkingDirectory, anyhow::Error> {
        let working_dir_holder = match &self.tmp {
            None => WorkingDirectory::Temporary(TempDir::with_prefix("spinup-")?),
            Some(d) => WorkingDirectory::Given(d.to_owned()),
        };
        if !working_dir_holder.path().exists() {
            std::fs::create_dir_all(working_dir_holder.path()).with_context(|| {
                format!(
                    "Could not create working directory '{}'",
                    working_dir_holder.path().display()
                )
            })?;
        }
        Ok(working_dir_holder)
    }

    async fn start_trigger_processes(
        self,
        trigger_cmds: Vec<Vec<String>>,
        run_opts: RunTriggerOpts,
    ) -> anyhow::Result<Vec<tokio::process::Child>> {
        let mut trigger_processes = Vec::with_capacity(trigger_cmds.len());

        for cmd in trigger_cmds {
            let child = self
                .start_trigger(cmd.clone(), Some(run_opts.clone()))
                .await
                .context("Failed to start trigger process")?;
            trigger_processes.push(child);
        }

        Ok(trigger_processes)
    }

    async fn start_trigger(
        &self,
        trigger_cmd: Vec<String>,
        opts: Option<RunTriggerOpts>,
    ) -> Result<tokio::process::Child, anyhow::Error> {
        // The docs for `current_exe` warn that this may be insecure because it could be executed
        // via hard-link. I think it should be fine as long as we aren't `setuid`ing this binary.
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(&trigger_cmd);

        if let Some(RunTriggerOpts {
            locked_url,
            working_dir,
            local_app_dir,
        }) = opts
        {
            cmd.env(SPIN_LOCKED_URL, locked_url)
                .env(SPIN_WORKING_DIR, &working_dir)
                .args(&self.trigger_args);

            if let Some(local_app_dir) = local_app_dir {
                cmd.env(SPIN_LOCAL_APP_DIR, local_app_dir);
            }

            cmd.kill_on_drop(true);
        } else {
            cmd.arg("--help-args-only");
        }

        tracing::trace!("Running trigger executor: {:?}", cmd);

        let child = cmd.spawn().context("Failed to execute trigger")?;
        Ok(child)
    }

    fn app_source(&self) -> AppSource {
        match (&self.app_source, &self.file_source, &self.registry_source) {
            (None, None, None) => self.default_manifest_or_none(),
            (Some(source), None, None) => AppSource::infer_source(source),
            (None, Some(file), None) => AppSource::infer_file_source(file.to_owned()),
            (None, None, Some(reference)) => AppSource::OciRegistry(reference.to_owned()),
            _ => AppSource::unresolvable("More than one application source was specified"),
        }
    }

    fn default_manifest_or_none(&self) -> AppSource {
        let default_manifest = PathBuf::from(DEFAULT_MANIFEST_FILE);
        if default_manifest.exists() {
            AppSource::File(default_manifest)
        } else if self.trigger_args_look_file_like() {
            let msg = format!(
                "Default file 'spin.toml' not found. Did you mean `spin up -f {}`?`",
                self.trigger_args[0].to_string_lossy()
            );
            AppSource::Unresolvable(msg)
        } else {
            AppSource::None
        }
    }

    fn trigger_args_look_file_like(&self) -> bool {
        // Heuristic for the user typing `spin up foo` instead of `spin up -f foo` - in the
        // first case `foo` gets interpreted as a trigger arg which is probably not what the
        // user intended.
        !self.trigger_args.is_empty() && !self.trigger_args[0].to_string_lossy().starts_with('-')
    }

    async fn write_locked_app(
        &self,
        locked_app: &LockedApp,
        working_dir: &Path,
    ) -> Result<String, anyhow::Error> {
        let locked_path = working_dir.join("spin.lock");
        let locked_app_contents =
            serde_json::to_vec_pretty(&locked_app).context("failed to serialize locked app")?;
        tokio::fs::write(&locked_path, locked_app_contents)
            .await
            .with_context(|| format!("failed to write {}", quoted_path(&locked_path)))?;
        let locked_url = Url::from_file_path(&locked_path)
            .map_err(|_| anyhow!("cannot convert to file URL: {}", quoted_path(&locked_path)))?
            .to_string();

        Ok(locked_url)
    }

    // Take the AppSource and do the minimum amount of work necessary to
    // be able to resolve the trigger executor.
    async fn resolve_app_source(
        &self,
        app_source: &AppSource,
        working_dir: &Path,
    ) -> anyhow::Result<ResolvedAppSource> {
        Ok(match &app_source {
            AppSource::File(path) => ResolvedAppSource::File {
                manifest_path: path.clone(),
                manifest: spin_manifest::manifest_from_file(path)?,
            },
            // TODO: We could make the `--help` experience a little faster if
            // we could fetch just the locked app JSON at this stage.
            AppSource::OciRegistry(reference) => {
                let mut client = spin_oci::Client::new(self.insecure, None)
                    .await
                    .context("cannot create registry client")?;

                let locked_app = OciLoader::new(working_dir)
                    .load_app(&mut client, reference)
                    .await?;
                ResolvedAppSource::OciRegistry { locked_app }
            }
            AppSource::Unresolvable(err) => bail!("{err}"),
            AppSource::None => bail!("Internal error - should have shown help"),
        })
    }

    // Finish preparing a ResolvedAppSource for execution.
    async fn load_resolved_app_source(
        &self,
        resolved: ResolvedAppSource,
        working_dir: &Path,
    ) -> anyhow::Result<LockedApp> {
        match resolved {
            ResolvedAppSource::File { manifest_path, .. } => {
                let files_mount_strategy = if self.direct_mounts {
                    FilesMountStrategy::Direct
                } else {
                    FilesMountStrategy::Copy(working_dir.join("assets"))
                };
                spin_loader::from_file(&manifest_path, files_mount_strategy, None)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to load manifest from {}",
                            quoted_path(&manifest_path)
                        )
                    })
            }
            ResolvedAppSource::OciRegistry { locked_app } => Ok(locked_app),
        }
    }

    fn update_locked_app(&self, locked_app: &mut LockedApp) {
        // Apply --env to component environments
        if !self.env.is_empty() {
            for component in locked_app.components.iter_mut() {
                component.env.extend(self.env.iter().cloned());
            }
        }
    }
}

#[cfg(windows)]
fn set_kill_on_ctrl_c(trigger_processes: &Vec<tokio::process::Child>) -> Result<(), anyhow::Error> {
    Ok(())
}

#[cfg(not(windows))]
fn set_kill_on_ctrl_c(trigger_processes: &[tokio::process::Child]) -> Result<(), anyhow::Error> {
    // https://github.com/nix-rust/nix/issues/656
    let pids = trigger_processes
        .iter()
        .flat_map(|child| child.id().map(|id| nix::unistd::Pid::from_raw(id as i32)))
        .collect_vec();
    ctrlc::set_handler(move || {
        for pid in &pids {
            if let Err(err) = nix::sys::signal::kill(*pid, nix::sys::signal::SIGTERM) {
                tracing::warn!("Failed to kill trigger handler process: {:?}", err)
            }
        }
    })?;
    Ok(())
}

#[derive(Clone)]
struct RunTriggerOpts {
    locked_url: String,
    working_dir: PathBuf,
    local_app_dir: Option<PathBuf>,
}

enum WorkingDirectory {
    Given(PathBuf),
    Temporary(TempDir),
}

impl WorkingDirectory {
    fn path(&self) -> &Path {
        match self {
            Self::Given(p) => p,
            Self::Temporary(t) => t.path(),
        }
    }
}

// Parse the environment variables passed in `key=value` pairs.
fn parse_env_var(s: &str) -> Result<(String, String)> {
    let parts: Vec<_> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        bail!("Environment variable must be of the form `key=value`");
    }
    Ok((parts[0].to_owned(), parts[1].to_owned()))
}

fn resolve_trigger_plugin(trigger_type: &str) -> Result<String> {
    use crate::commands::plugins::PluginCompatibility;
    use spin_plugins::manager::PluginManager;

    let subcommand = format!("trigger-{trigger_type}");
    let plugin_manager = PluginManager::try_default()
        .with_context(|| format!("Failed to access plugins looking for '{subcommand}'"))?;
    let plugin_store = plugin_manager.store();
    let is_installed = plugin_store
        .installed_manifests()
        .unwrap_or_default()
        .iter()
        .any(|m| m.name() == subcommand);

    if is_installed {
        return Ok(subcommand);
    }

    if let Some(known) = plugin_store
        .catalogue_manifests()
        .unwrap_or_default()
        .iter()
        .find(|m| m.name() == subcommand)
    {
        match PluginCompatibility::for_current(known) {
            PluginCompatibility::Compatible => Err(anyhow!("No built-in trigger named '{trigger_type}', but plugin '{subcommand}' is available to install")),
            _ => Err(anyhow!("No built-in trigger named '{trigger_type}', and plugin '{subcommand}' is not compatible"))
        }
    } else {
        Err(anyhow!("No built-in trigger named '{trigger_type}', and no plugin named '{subcommand}' was found"))
    }
}

fn trigger_command(trigger_type: &str) -> Vec<String> {
    vec!["trigger".to_owned(), trigger_type.to_owned()]
}

fn trigger_command_for_resolved_app_source(
    resolved: &ResolvedAppSource,
) -> Result<Vec<Vec<String>>> {
    let trigger_type = resolved.trigger_types()?;
    trigger_type
        .iter()
        .map(|&t| match t {
            "http" | "redis" => Ok(trigger_command(t)),
            _ => {
                let cmd = resolve_trigger_plugin(t)?;
                Ok(vec![cmd])
            }
        })
        .collect()
}

#[cfg(test)]
mod test {
    use crate::commands::up::app_source::AppSource;

    use super::*;

    fn repo_path(path: &str) -> String {
        // This is all strings and format because app_source is a string not a PathBuf
        let repo_base = env!("CARGO_MANIFEST_DIR");
        format!("{repo_base}/{path}")
    }

    #[test]
    fn can_infer_files() {
        let file = repo_path("examples/http-rust/spin.toml");

        let source = UpCommand {
            app_source: Some(file.clone()),
            ..Default::default()
        }
        .app_source();

        assert_eq!(AppSource::File(PathBuf::from(file)), source);
    }

    #[test]
    fn can_infer_directories() {
        let dir = repo_path("examples/http-rust");

        let source = UpCommand {
            app_source: Some(dir.clone()),
            ..Default::default()
        }
        .app_source();

        assert_eq!(
            AppSource::File(PathBuf::from(dir).join("spin.toml")),
            source
        );
    }

    #[test]
    fn reject_nonexistent_files() {
        let file = repo_path("src/commands/biscuits.toml");

        let source = UpCommand {
            app_source: Some(file),
            ..Default::default()
        }
        .app_source();

        assert!(matches!(source, AppSource::Unresolvable(_)));
    }

    #[test]
    fn reject_nonexistent_files_relative_path() {
        let file = "zoink/honk/biscuits.toml".to_owned(); // NOBODY CREATE THIS OKAY

        let source = UpCommand {
            app_source: Some(file),
            ..Default::default()
        }
        .app_source();

        assert!(matches!(source, AppSource::Unresolvable(_)));
    }

    #[test]
    fn reject_unsuitable_directories() {
        let dir = repo_path("src/commands");

        let source = UpCommand {
            app_source: Some(dir),
            ..Default::default()
        }
        .app_source();

        assert!(matches!(source, AppSource::Unresolvable(_)));
    }

    #[test]
    fn can_infer_oci_registry_reference() {
        let reference = "ghcr.io/fermyon/noodles:v1".to_owned();

        let source = UpCommand {
            app_source: Some(reference.clone()),
            ..Default::default()
        }
        .app_source();

        assert_eq!(AppSource::OciRegistry(reference), source);
    }

    #[test]
    fn can_infer_docker_registry_reference() {
        // Testing that the magic docker heuristic doesn't misfire here.
        let reference = "docker.io/fermyon/noodles".to_owned();

        let source = UpCommand {
            app_source: Some(reference.clone()),
            ..Default::default()
        }
        .app_source();

        assert_eq!(AppSource::OciRegistry(reference), source);
    }

    #[test]
    fn can_reject_complete_gibberish() {
        let garbage = repo_path("ftp://🤡***🤡 HELLO MR CLOWN?!");

        let source = UpCommand {
            app_source: Some(garbage),
            ..Default::default()
        }
        .app_source();

        // Honestly I feel Unresolvable might be a bit weak sauce for this case
        assert!(matches!(source, AppSource::Unresolvable(_)));
    }

    #[test]
    fn parses_untyped_source() {
        UpCommand::try_parse_from(["up", "-f", "ghcr.io/example/test:v1"])
            .expect("Failed to parse --from with option");
        UpCommand::try_parse_from(["up", "-f", "ghcr.io/example/test:v1", "--direct-mounts"])
            .expect("Failed to parse --from with option");
        UpCommand::try_parse_from([
            "up",
            "-f",
            "ghcr.io/example/test:v1",
            "--listen",
            "127.0.0.1:39453",
        ])
        .expect("Failed to parse --from with trigger option");
    }

    #[test]
    fn parses_typed_source() {
        UpCommand::try_parse_from(["up", "--from-registry", "ghcr.io/example/test:v1"])
            .expect("Failed to parse --from-registry with option");
        UpCommand::try_parse_from([
            "up",
            "--from-registry",
            "ghcr.io/example/test:v1",
            "--direct-mounts",
        ])
        .expect("Failed to parse --from-registry with option");
        UpCommand::try_parse_from([
            "up",
            "--from-registry",
            "ghcr.io/example/test:v1",
            "--listen",
            "127.0.0.1:39453",
        ])
        .expect("Failed to parse --from-registry with trigger option");
    }

    #[test]
    fn parses_implicit_source() {
        UpCommand::try_parse_from(["up"]).expect("Failed to parse implicit source with option");
        UpCommand::try_parse_from(["up", "--direct-mounts"])
            .expect("Failed to parse implicit source with option");
        UpCommand::try_parse_from(["up", "--listen", "127.0.0.1:39453"])
            .expect("Failed to parse implicit source with trigger option");
    }
}
