// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;

use anyhow::{Context, Result};
use args::{ProfilerCommand, ProfilerSubCommand};
use async_fs::File;
use core::fmt;
use errors::{ffx_bail, ffx_error};
use fdomain_client::fidl::Proxy;
use fdomain_fuchsia_cpu_profiler as profiler;
use fdomain_fuchsia_cpu_profiler::SessionResult;
use fdomain_fuchsia_test_manager as test_manager;
use ffx_config::EnvironmentContext;
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{FfxMain, FfxTool, bug, deferred, return_user_error, user_error};
use log::info;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{BufRead, IsTerminal, stdin};
use std::time::Duration;
use target_holders::moniker;
use tempfile::Builder;
use termion::{color, style};

#[derive(Serialize, JsonSchema)]
pub struct ShowCpuProfilerCmd {
    pub samples_collected: Option<u64>,
    pub median_sample_time: Option<u64>,
    pub mean_sample_time: Option<u64>,
    pub max_sample_time: Option<u64>,
    pub min_sample_time: Option<u64>,
    pub missing_process_mappings: Option<Vec<u64>>,
}

impl fmt::Display for ShowCpuProfilerCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session Stats: \n")?;
        if let Some(num_samples) = self.samples_collected {
            write!(f, "    Number of samples collected: {}\n", num_samples)?;
        }
        if let Some(median_sample_time) = self.median_sample_time {
            write!(f, "    Median sample time: {}us\n", median_sample_time)?;
        }
        if let Some(mean_sample_time) = self.mean_sample_time {
            write!(f, "    Mean sample time: {}us\n", mean_sample_time)?;
        }
        if let Some(max_sample_time) = self.max_sample_time {
            write!(f, "    Max sample time: {}us\n", max_sample_time)?;
        }
        if let Some(min_sample_time) = self.min_sample_time {
            write!(f, "    Min sample time: {}us\n", min_sample_time)?;
        }
        if let Some(ref pids) = self.missing_process_mappings {
            write!(f, "    Processes missing mappings: {:?}\n", pids)?;
        }
        Ok(())
    }
}

type Writer = MachineWriter<ShowCpuProfilerCmd>;
#[derive(FfxTool)]
pub struct ProfilerTool {
    #[with(deferred(moniker("/core/cpu_profiler")))]
    controller: fho::Deferred<profiler::SessionProxy>,
    #[with(deferred(moniker("/core/cpu_profiler/profiler_session_manager")))]
    session_manager: fho::Deferred<profiler::SessionManagerProxy>,
    #[command]
    cmd: ProfilerCommand,
    context: EnvironmentContext,
}

impl FfxMain for ProfilerTool {
    type Writer = Writer;

    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        info!(cmd:? = self.cmd; "Running profiler... ");
        self.profiler(writer).await
    }
}

fn gather_targets(opts: &args::Attach) -> Result<profiler::TargetConfig> {
    if let Some(moniker) = &opts.moniker {
        if !opts.pids.is_empty()
            || !opts.tids.is_empty()
            || !opts.job_ids.is_empty()
            || opts.system_wide
        {
            ffx_bail!(
                "Targeting both a component and specific jobs/processes/threads is not supported"
            )
        }
        let component_config = profiler::AttachConfig::AttachToComponentMoniker(moniker.clone());
        Ok(profiler::TargetConfig::Component(component_config))
    } else if let Some(url) = &opts.url {
        if !opts.pids.is_empty()
            || !opts.tids.is_empty()
            || !opts.job_ids.is_empty()
            || opts.system_wide
        {
            ffx_bail!(
                "Targeting both a component and specific jobs/processes/threads is not supported"
            )
        }
        let component_config = profiler::AttachConfig::AttachToComponentUrl(url.clone());
        Ok(profiler::TargetConfig::Component(component_config))
    } else {
        let mut tasks: Vec<_> = opts
            .job_ids
            .iter()
            .map(|&id| profiler::Task::Job(id))
            .chain(opts.pids.iter().map(|&id| profiler::Task::Process(id)))
            .chain(opts.tids.iter().map(|&id| profiler::Task::Thread(id)))
            .collect();
        if opts.system_wide {
            tasks.push(profiler::Task::SystemWide(profiler::SystemWide {}));
        }
        if tasks.is_empty() {
            ffx_bail!("No targets were specified")
        }
        Ok(profiler::TargetConfig::Tasks(tasks))
    }
}

fn make_boot_target_config(
    target_config: &profiler::TargetConfig,
) -> Result<profiler::OnBootTargetConfig> {
    match target_config {
        profiler::TargetConfig::Component(profiler::AttachConfig::AttachToComponentMoniker(
            moniker,
        )) => Ok(profiler::OnBootTargetConfig::Component(
            profiler::OnBootAttachConfig::AttachToComponentMoniker(moniker.clone()),
        )),
        profiler::TargetConfig::Component(profiler::AttachConfig::AttachToComponentUrl(url)) => {
            Ok(profiler::OnBootTargetConfig::Component(
                profiler::OnBootAttachConfig::AttachToComponentUrl(url.clone()),
            ))
        }
        profiler::TargetConfig::Tasks(tasks) => {
            Ok(profiler::OnBootTargetConfig::Tasks(tasks.clone()))
        }
        _ => ffx_bail!("On-boot profiling requires a component URL or moniker, or tasks"),
    }
}

#[derive(Debug, PartialEq)]
struct SessionOpts {
    symbolize: bool,
    buffer_size_mb: Option<u64>,
    print_stats: bool,
    pprof_conversion: bool,
    output_format: args::OutputFormat,
    output: String,
    duration: Option<u64>,
    color_output: bool,
}

fn check_background_args(
    duration: Option<u64>,
    output: Option<&str>,
    print_stats: bool,
    symbolize: bool,
    pprof_conversion: bool,
    output_format: args::OutputFormat,
    color_output: bool,
) -> fho::Result<()> {
    if duration.is_some() {
        ffx_bail!("Cannot specify a duration when starting a background profiling session.");
    }

    // Check for non-default values for the other arguments.
    if output.is_some()
        || print_stats
        || !symbolize
        || !pprof_conversion
        || output_format != args::OutputFormat::Pprof
        || color_output != std::io::stdout().is_terminal()
    {
        let message: &str = "The options --output, --print-stats, --symbolize, --pprof-conversion, \
            --output-format, and --color-output should be specified when calling `ffx profiler stop` \
            when using a background profiling session.";
        return_user_error!("{message}");
    }
    Ok(())
}

fn default_sampling_config(
    sample_period_us: u64,
    unwind_strategy: args::UnwindStrategy,
) -> profiler::SamplingConfig {
    let strategy = match unwind_strategy {
        args::UnwindStrategy::Dwarf => profiler::CallgraphStrategy::Dwarf,
        args::UnwindStrategy::FramePointer => profiler::CallgraphStrategy::FramePointer,
    };

    profiler::SamplingConfig {
        period: Some(sample_period_us * 1000),
        timebase: Some(profiler::Counter::PlatformIndependent(profiler::CounterId::Nanoseconds)),
        sample: Some(profiler::Sample {
            callgraph: Some(profiler::CallgraphConfig {
                strategy: Some(strategy),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn create_session_opts(
    symbolize: bool,
    buffer_size_mb: Option<u64>,
    print_stats: bool,
    output: Option<String>,
    duration: Option<u64>,
    pprof_conversion: bool,
    output_format: args::OutputFormat,
    color_output: bool,
) -> SessionOpts {
    let extension = if !symbolize {
        "fxt"
    } else {
        match output_format {
            args::OutputFormat::Pprof => "pb",
            args::OutputFormat::Perfetto => "pftrace",
            args::OutputFormat::Fxt => "fxt",
        }
    };
    let final_output = match output {
        Some(o) => o,
        None => format!("profile.{}", extension),
    };
    SessionOpts {
        symbolize,
        buffer_size_mb,
        print_stats,
        output: final_output,
        duration,
        pprof_conversion,
        output_format,
        color_output,
    }
}

fn retained_raw_fxt_path(output: &std::path::Path) -> std::path::PathBuf {
    if let Some(s) = output.to_str() {
        if let Some(stem) = s.strip_suffix(".perfetto.fxt") {
            if !stem.is_empty() && !stem.ends_with('/') {
                return std::path::PathBuf::from(format!("{stem}.raw.fxt"));
            }
        }
    }
    let candidate = output.with_extension("raw.fxt");
    if candidate == output {
        let mut os = output.as_os_str().to_os_string();
        os.push(".raw.fxt");
        std::path::PathBuf::from(os)
    } else {
        candidate
    }
}

/// Symbolizes `unsymbolized_samples` and writes the result to `output` in `output_format`.
fn write_symbolized_profile(
    unsymbolized_samples: ffx_profiler::symbolize::UnsymbolizedSamples,
    output: &std::path::Path,
    pprof_conversion: bool,
    output_format: args::OutputFormat,
    context: &EnvironmentContext,
) -> Result<()> {
    let symbolized_records = unsymbolized_samples
        .process_unsymbolized_samples(output, pprof_conversion, context)
        .map_err(|e| anyhow::anyhow!("Failed to symbolize profile: {:?}", e))?;
    match output_format {
        args::OutputFormat::Pprof => {
            ffx_profiler::pprof::samples_to_pprof(symbolized_records, output.to_path_buf())
                .context("Failed to convert to pprof")?;
        }
        args::OutputFormat::Perfetto => {
            let bytes = ffx_profiler::perfetto::encode_perf_samples(&symbolized_records);
            std::fs::write(output, bytes)?;
        }
        args::OutputFormat::Fxt => {
            let bytes = ffx_profiler::perfetto::encode_perf_samples(&symbolized_records);
            std::fs::write(
                output,
                ffx_profiler::fxt_blob::wrap_perfetto_blobs(&bytes, "ffx_profiler"),
            )?;
        }
    }
    Ok(())
}

fn symbolize_file(opts: &args::Symbolize, context: &EnvironmentContext) -> fho::Result<()> {
    let unsymbolized_samples = ffx_profiler::symbolize::create_unsymbolized_samples(&opts.input)
        .map_err(|e| bug!("Failed to create unsymbolized samples: {:?}", e))?;
    write_symbolized_profile(
        unsymbolized_samples,
        &opts.output,
        opts.pprof_conversion,
        opts.output_format,
        context,
    )
    .map_err(|e| user_error!("{e:?}"))
}

fn print_stop_stats(
    writer: &mut Writer,
    stats: &ShowCpuProfilerCmd,
    color_output: bool,
    print_stats: bool,
) -> fho::Result<()> {
    if let Some(pids) = &stats.missing_process_mappings {
        if !pids.is_empty() {
            writeln!(
                writer.stderr(),
                "{}[WARNING] Failed to get symbols for some processes: {:?}\n\
                This can occur when processes exit before the profiler is able to read their modules.{}",
                if color_output {
                    format!("{}", color::Fg(color::Red))
                } else {
                    String::from("")
                },
                pids,
                if color_output { format!("{}", style::Reset) } else { String::from("") },
            ).map_err(|e| anyhow::anyhow!(e))?;
        }
    }
    if print_stats {
        writer.machine(stats)?;
        writer.line(format!("\n{stats}"))?;
    }
    Ok(())
}

pub enum ProfilerSessionManagerProxy {
    Controller(profiler::SessionProxy),
    SessionManager(profiler::SessionManagerProxy),
}

impl From<profiler::SessionProxy> for ProfilerSessionManagerProxy {
    fn from(proxy: profiler::SessionProxy) -> Self {
        Self::Controller(proxy)
    }
}

impl From<profiler::SessionManagerProxy> for ProfilerSessionManagerProxy {
    fn from(proxy: profiler::SessionManagerProxy) -> Self {
        Self::SessionManager(proxy)
    }
}

impl ProfilerSessionManagerProxy {
    pub fn domain(&self) -> std::sync::Arc<fdomain_client::Client> {
        match self {
            Self::Controller(c) => c.domain(),
            Self::SessionManager(mgr) => mgr.domain(),
        }
    }

    pub async fn configure(
        &self,
        req: profiler::SessionConfigureRequest,
    ) -> Result<Result<(), profiler::SessionConfigureError>, fidl::Error> {
        match self {
            Self::Controller(c) => c.configure(req).await,
            Self::SessionManager(mgr) => mgr.configure(req).await,
        }
    }

    pub async fn start(
        &self,
        req: &profiler::SessionStartRequest,
    ) -> Result<Result<(), profiler::SessionStartError>, fidl::Error> {
        match self {
            Self::Controller(c) => c.start(req).await,
            Self::SessionManager(mgr) => mgr.start(req).await,
        }
    }

    pub async fn stop(&self) -> Result<profiler::SessionResult, fidl::Error> {
        match self {
            Self::Controller(c) => c.stop().await,
            Self::SessionManager(mgr) => mgr.stop().await,
        }
    }

    pub async fn reset(&self) -> Result<(), fidl::Error> {
        match self {
            Self::Controller(c) => c.reset().await,
            Self::SessionManager(mgr) => mgr.reset().await,
        }
    }

    pub async fn start_session(
        &self,
        req: profiler::SessionManagerStartSessionRequest,
    ) -> Result<
        Result<profiler::SessionManagerStartSessionResponse, profiler::ManagerError>,
        fidl::Error,
    > {
        match self {
            Self::Controller(_) => Ok(Err(profiler::ManagerError::Start)),
            Self::SessionManager(mgr) => mgr.start_session(req).await,
        }
    }

    pub async fn stop_session(
        &self,
        req: profiler::SessionManagerStopSessionRequest,
    ) -> Result<Result<profiler::SessionResult, profiler::ManagerError>, fidl::Error> {
        match self {
            Self::Controller(_) => Ok(Err(profiler::ManagerError::Stop)),
            Self::SessionManager(mgr) => mgr.stop_session(req).await,
        }
    }

    pub async fn status(
        &self,
    ) -> Result<Result<profiler::SessionManagerStatusResponse, profiler::ManagerError>, fidl::Error>
    {
        match self {
            Self::Controller(_) => Ok(Err(profiler::ManagerError::NoSuchTask)),
            Self::SessionManager(mgr) => mgr.status().await,
        }
    }

    pub async fn abort_session(
        &self,
        req: &profiler::SessionManagerAbortSessionRequest,
    ) -> Result<Result<(), profiler::ManagerError>, fidl::Error> {
        match self {
            Self::Controller(_) => Ok(Err(profiler::ManagerError::Stop)),
            Self::SessionManager(mgr) => mgr.abort_session(req).await,
        }
    }

    pub async fn start_session_on_boot(
        &self,
        req: &profiler::SessionManagerStartSessionOnBootRequest,
    ) -> Result<Result<(), profiler::ManagerError>, fidl::Error> {
        match self {
            Self::Controller(_) => Ok(Err(profiler::ManagerError::Start)),
            Self::SessionManager(mgr) => mgr.start_session_on_boot(req).await,
        }
    }
}

async fn finalize_profile_session(
    proxy: &ProfilerSessionManagerProxy,
    writer: &mut Writer,
    options: &SessionOpts,
    copy_task: Option<fuchsia_async::Task<std::io::Result<u64>>>,
    unsymbolized_path: &std::path::PathBuf,
    context: &EnvironmentContext,
    is_background: bool,
) -> Result<()> {
    info!("Stopping profiler...");
    let mut copy_task = copy_task;

    let stats = if is_background {
        let (client, server) = proxy.domain().create_stream_socket();

        let req = profiler::SessionManagerStopSessionRequest {
            output: Some(server),
            ..Default::default()
        };

        let mut output = async_fs::File::create(&unsymbolized_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create output file: {:?}", e))?;

        copy_task = Some(fuchsia_async::Task::local(async move {
            futures::io::copy(client, &mut output).await
        }));

        match proxy.stop_session(req).await? {
            Ok(stats) => SessionResult {
                samples_collected: stats.samples_collected,
                mean_sample_time: stats.mean_sample_time,
                median_sample_time: stats.median_sample_time,
                min_sample_time: stats.min_sample_time,
                max_sample_time: stats.max_sample_time,
                missing_process_mappings: stats.missing_process_mappings,
                ..Default::default()
            },
            Err(e) => anyhow::bail!("Failed to stop session: {:?}", e),
        }
    } else {
        proxy.stop().await?
    };

    let out_stats = ShowCpuProfilerCmd {
        samples_collected: stats.samples_collected,
        median_sample_time: stats.median_sample_time,
        mean_sample_time: stats.mean_sample_time,
        max_sample_time: stats.max_sample_time,
        min_sample_time: stats.min_sample_time,
        missing_process_mappings: stats.missing_process_mappings,
    };

    print_stop_stats(writer, &out_stats, options.color_output, options.print_stats)?;

    info!("Profiler stopped, waiting for copy to complete...");
    if let Some(task) = copy_task {
        task.await.map_err(|e| anyhow::anyhow!(e))?;
    }

    info!("Copy from profiler completed, resetting profiler...");
    proxy.reset().await?;

    let unsymbolized_samples =
        ffx_profiler::symbolize::create_unsymbolized_samples(unsymbolized_path)?;

    if !options.symbolize {
        std::fs::write(unsymbolized_path, format!("{unsymbolized_samples:#?}\n"))?;
        return Ok(());
    }

    let output_path = std::path::Path::new(&options.output);
    if matches!(options.output_format, args::OutputFormat::Perfetto | args::OutputFormat::Fxt) {
        // Keep the raw capture next to the Perfetto output so the other container can be
        // produced later with `ffx profiler symbolize`.
        let raw_fxt_path = retained_raw_fxt_path(output_path);
        if unsymbolized_path.as_path() != raw_fxt_path.as_path() {
            std::fs::copy(unsymbolized_path, &raw_fxt_path).with_context(|| {
                format!(
                    "Failed to retain raw FXT profile from {:?} to {:?}",
                    unsymbolized_path, raw_fxt_path
                )
            })?;
        }
    }
    write_symbolized_profile(
        unsymbolized_samples,
        output_path,
        options.pprof_conversion,
        options.output_format,
        context,
    )
}

async fn run_session(
    context: &EnvironmentContext,
    session_proxy: &ProfilerSessionManagerProxy,
    mut writer: Writer,
    config: profiler::Config,
    opts: SessionOpts,
) -> fho::Result<()> {
    info!(config:? = config, opts:? = opts; "Running profiler session...");
    let (client, server) = session_proxy.domain().create_stream_socket();

    session_proxy
        .configure(profiler::SessionConfigureRequest {
            output: Some(server),
            config: Some(config),
            ..Default::default()
        })
        .await
        .map_err(|e| bug!("{e}"))?
        .map_err(|e| user_error!("Failed to start: {:?}", e))?;
    info!("Profiler session is configured.");

    let tmp_dir = Builder::new()
        .prefix("fuchsia_cpu_profiler_")
        .tempdir()
        .map_err(|e| user_error!("Failed to create temporary directory: {e}"))?;
    let unsymbolized_path = if opts.symbolize {
        tmp_dir.path().join("unsymbolized.txt")
    } else {
        std::path::PathBuf::from(&opts.output)
    };

    let mut output = File::create(&unsymbolized_path)
        .await
        .map_err(|e| user_error!("Failed to create output file: {e}"))?;
    let copy_task =
        fuchsia_async::Task::local(async move { futures::io::copy(client, &mut output).await });

    info!("Starting profiler...");
    session_proxy
        .start(&profiler::SessionStartRequest {
            buffer_results: Some(true),
            buffer_size_mb: opts.buffer_size_mb,
            ..Default::default()
        })
        .await
        .map_err(|e| bug!(e))?
        .map_err(|e| ffx_error!("Failed to start: {:?}", e))?;
    info!("Profiler started.");

    if let &Some(duration) = &opts.duration {
        writer.line(format!("Waiting for {} seconds...", duration))?;
        fuchsia_async::Timer::new(Duration::from_secs(duration)).await;
    } else {
        writer.line("Press <enter> to stop profiling...")?;
        blocking::unblock(|| {
            let _ = stdin().lock().read_line(&mut String::new());
        })
        .await;
    }

    finalize_profile_session(
        session_proxy,
        &mut writer,
        &opts,
        Some(copy_task),
        &unsymbolized_path,
        context,
        false,
    )
    .await?;

    Ok(())
}

async fn download_android_symbols(
    opts: args::DownloadAndroidSymbols,
    context: &EnvironmentContext,
) -> Result<()> {
    info!("Fetching android symbols for build {} target {}", opts.bid, opts.target);

    let target_prefix = opts.target.split('-').next().unwrap_or(&opts.target);
    let glob = format!("{}-symbols-*.zip", target_prefix);

    // Create a temporary directory to download the zip into
    let tmp_dir = Builder::new().prefix("android_symbols_").tempdir()?;
    let out_dir = tmp_dir.path().join("symbols");
    std::fs::create_dir_all(&out_dir)?;

    // Invoke fetch_artifact to get the symbol archive
    let mut command = std::process::Command::new("/google/data/ro/projects/android/fetch_artifact");
    command
        .arg("--bid")
        .arg(&opts.bid)
        .arg("--target")
        .arg(&opts.target)
        .arg("--use_shared_quota")
        .arg(&glob)
        .arg(&out_dir);

    println!("Running: {:?}", command);
    let status = command.status()?;

    if !status.success() {
        anyhow::bail!("fetch_artifact failed: {}", status);
    }

    // Now extract the zip(s) into a sub-directory
    let extract_dir = tmp_dir.path().join("extracted");
    std::fs::create_dir_all(&extract_dir)?;

    println!("Extracting symbols...");
    for entry in std::fs::read_dir(&out_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("zip") {
            println!("Unzipping {}", path.display());
            let extract_status = std::process::Command::new("unzip")
                .arg("-q")
                .arg(&path)
                .arg("-d")
                .arg(&extract_dir)
                .status()?;

            if !extract_status.success() {
                anyhow::bail!("unzip failed: {}", extract_status);
            }
        }
    }

    // Determine the user's .build-id directory
    let home = context.get_home_dir().context("Failed to get HOME directory")?;
    let build_id_dir = home.join(".fuchsia/debug/build-id");
    std::fs::create_dir_all(&build_id_dir)?;

    println!("Populating .build-id cache...");

    // Traverse extracted directory to find all .so files
    let mut dirs_to_visit = vec![extract_dir];
    while let Some(dir) = dirs_to_visit.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs_to_visit.push(path);
                continue;
            }
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("so") {
                continue;
            }

            // Get Build ID using readelf
            let Ok(output) = std::process::Command::new("readelf").arg("-n").arg(&path).output()
            else {
                continue;
            };

            if !output.status.success() {
                continue;
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let Some(line) = stdout.lines().find(|l| l.contains("Build ID:")) else {
                continue;
            };

            let Some(build_id) = line.split_whitespace().last() else {
                continue;
            };

            if build_id.len() <= 2 {
                continue;
            }

            let (xx, yy) = build_id.split_at(2);
            let target_dir = build_id_dir.join(xx);
            if std::fs::create_dir_all(&target_dir).is_ok() {
                let target_file = target_dir.join(format!("{}.debug", yy));
                let _ = std::fs::copy(&path, &target_file);
            }
        }
    }

    println!("Successfully downloaded and cached Android symbols for bid {}!", opts.bid);

    Ok(())
}

impl ProfilerTool {
    async fn get_session_proxy(self) -> Result<ProfilerSessionManagerProxy> {
        match self.session_manager.await {
            Ok(p) => Ok(p.into()),
            Err(e) => {
                log::warn!(
                    "Cannot connect to SessionManagerProxy, falling back to basic profiler controller: {e:?}."
                );
                Ok(self.controller.await?.into())
            }
        }
    }

    pub async fn profiler(self, mut writer: Writer) -> fho::Result<()> {
        let context = self.context.clone();
        let sub_command = self.cmd.sub_cmd.clone();

        // Symbolizing an existing capture runs entirely on the host, so don't require a
        // target connection for it.
        if let ProfilerSubCommand::Symbolize(opts) = &sub_command {
            return symbolize_file(opts, &context);
        }

        let session_proxy = self.get_session_proxy().await?;

        let (targets, config, session_opts, background) = match sub_command {
            ProfilerSubCommand::Attach(opts) => {
                let target = gather_targets(&opts)?;
                if opts.on_boot {
                    let boot_target = make_boot_target_config(&target)?;
                    let config = profiler::OnBootConfig {
                        configs: Some(vec![default_sampling_config(
                            opts.sample_period_us,
                            opts.unwind_strategy.clone(),
                        )]),
                        target: Some(boot_target),
                        ..Default::default()
                    };

                    if let ProfilerSessionManagerProxy::SessionManager(mgr) = &session_proxy {
                        let req = profiler::SessionManagerStartSessionOnBootRequest {
                            config: Some(config),
                            ..Default::default()
                        };
                        match mgr.start_session_on_boot(&req).await.map_err(|e| bug!(e))? {
                            Ok(_) => {
                                writer.line("On-boot profiling session configured. \
                                             Profiles will be collected when the target component starts.")?;
                            }
                            Err(e) => {
                                ffx_bail!("Failed to start on-boot session: {:?}", e);
                            }
                        }
                        return Ok(());
                    } else {
                        ffx_bail!(
                            "On-boot profiling requires a SessionManager connection, but none could be established."
                        );
                    }
                }

                if opts.background {
                    check_background_args(
                        opts.duration,
                        opts.output.as_deref(),
                        opts.print_stats,
                        opts.symbolize,
                        opts.pprof_conversion,
                        opts.output_format,
                        opts.color_output,
                    )?;
                }
                let config = default_sampling_config(opts.sample_period_us, opts.unwind_strategy);
                let session_opts = create_session_opts(
                    opts.symbolize,
                    opts.buffer_size_mb,
                    opts.print_stats,
                    opts.output,
                    opts.duration,
                    opts.pprof_conversion,
                    opts.output_format,
                    opts.color_output,
                );
                (target, config, session_opts, opts.background)
            }
            ProfilerSubCommand::Launch(opts) => {
                if opts.background {
                    check_background_args(
                        opts.duration,
                        opts.output.as_deref(),
                        opts.print_stats,
                        opts.symbolize,
                        opts.pprof_conversion,
                        opts.output_format,
                        opts.color_output,
                    )?;
                }
                let component_config = if opts.test {
                    profiler::AttachConfig::LaunchTest(profiler::LaunchTest {
                        url: Some(opts.url.clone()),
                        options: Some(test_manager::RunSuiteOptions {
                            test_case_filters: Some(opts.test_filters),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                } else {
                    profiler::AttachConfig::LaunchComponent(profiler::LaunchComponent {
                        url: Some(opts.url.clone()),
                        moniker: opts.moniker.clone(),
                        ..Default::default()
                    })
                };
                let target = profiler::TargetConfig::Component(component_config);
                let config = default_sampling_config(opts.sample_period_us, opts.unwind_strategy);
                let session_opts = create_session_opts(
                    opts.symbolize,
                    opts.buffer_size_mb,
                    opts.print_stats,
                    opts.output,
                    opts.duration,
                    opts.pprof_conversion,
                    opts.output_format,
                    opts.color_output,
                );
                (target, config, session_opts, opts.background)
            }
            ProfilerSubCommand::Symbolize(_) => {
                unreachable!("handled above without a target connection")
            }
            ProfilerSubCommand::DownloadAndroidSymbols(opts) => {
                return download_android_symbols(opts, &context)
                    .await
                    .map_err(|e| user_error!("Failed to download Android symbols: {e:?}"));
            }
            ProfilerSubCommand::Stop(stop_options) => {
                if let ProfilerSessionManagerProxy::SessionManager(mgr) = &session_proxy {
                    if stop_options.abort {
                        let req = profiler::SessionManagerAbortSessionRequest::default();
                        info!("Aborting background session");

                        match mgr
                            .abort_session(&req)
                            .await
                            .map_err(|e| bug!("Failed to abort session: {:?}", e))?
                        {
                            Ok(_) => writer.line("Session aborted successfully.")?,
                            Err(e) => ffx_bail!("Failed to abort background session: {:?}", e),
                        }
                        return Ok(());
                    }

                    let session_opts = create_session_opts(
                        stop_options.symbolize,
                        None,
                        stop_options.print_stats,
                        stop_options.output,
                        None,
                        stop_options.pprof_conversion,
                        stop_options.output_format,
                        stop_options.color_output,
                    );

                    info!("Stopping background session");

                    // The temp dir needs to stay in scope until symbolization is completed.
                    let tmp_dir = Builder::new()
                        .prefix("fuchsia_cpu_profiler_")
                        .tempdir()
                        .map_err(|e| bug!("Failed to create temp dir: {:?}", e))?;

                    let unsymbolized_path = if stop_options.symbolize {
                        tmp_dir.path().join("unsymbolized.txt")
                    } else {
                        std::path::PathBuf::from(&session_opts.output)
                    };

                    finalize_profile_session(
                        &session_proxy,
                        &mut writer,
                        &session_opts,
                        None,
                        &unsymbolized_path,
                        &context,
                        true,
                    )
                    .await
                    .map_err(|e| user_error!("Failed to finalize session: {:?}", e))?;

                    writer.line(format!("Wrote profile to {}", session_opts.output))?;
                } else {
                    ffx_bail!("Connecting to SessionManager is required for background profiling.");
                }
                return Ok(());
            }
            ProfilerSubCommand::Status(_) => {
                if let ProfilerSessionManagerProxy::SessionManager(mgr) = session_proxy {
                    let response = mgr
                        .status()
                        .await
                        .map_err(|e| bug!("Failed to get background session status: {:?}", e))?;
                    match response {
                        Ok(resp) => {
                            if let Some(sessions) = resp.sessions {
                                if sessions.is_empty() {
                                    writer.line("No background profiling sessions active.")?;
                                } else {
                                    writer.line("Active background sessions:")?;
                                    for session in sessions {
                                        writer.line(format!(
                                            "  - task_id: {}",
                                            session.task_id.unwrap_or(0)
                                        ))?;
                                    }
                                }
                            } else {
                                writer.line("No background profiling sessions active.")?;
                            }
                        }
                        Err(e) => {
                            ffx_bail!("Failed to get background session status: {:?}", e);
                        }
                    }
                } else {
                    ffx_bail!("Connecting to SessionManager is required for background profiling.");
                }
                return Ok(());
            }
        };
        let config = profiler::Config {
            configs: Some(vec![config]),
            target: Some(targets),
            ..Default::default()
        };

        if background {
            if let ProfilerSessionManagerProxy::SessionManager(mgr) = &session_proxy {
                info!(config:? = config; "Starting background profiler session...");

                let req = profiler::SessionManagerStartSessionRequest {
                    config: Some(config),
                    ..Default::default()
                };
                let response = mgr.start_session(req).await.map_err(|e| bug!(e))?;
                match response {
                    Ok(resp) => {
                        writer.line(format!(
                            "Background session started. task_id: {}",
                            resp.task_id.unwrap_or(0)
                        ))?;
                    }
                    Err(e) => {
                        ffx_bail!("Failed to start background session: {:?}", e);
                    }
                }
                Ok(())
            } else {
                ffx_bail!(
                    "Background profiling requires a SessionManager connection, but none could be established."
                );
            }
        } else {
            run_session(&context, &session_proxy, writer, config, session_opts).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_gather_targets() {
        let args = args::Attach {
            pids: vec![1, 2, 3],
            tids: vec![4, 5, 6],
            job_ids: vec![7, 8, 9],
            url: None,
            buffer_size_mb: Some(8 as u64),
            moniker: None,
            duration: None,
            output: Some(String::from("output_file.pb")),
            ..Default::default()
        };
        let target = gather_targets(&args);
        match target {
            Ok(profiler::TargetConfig::Tasks(vec)) => assert!(vec.len() == 9),
            _ => assert!(false),
        }

        let empty_args = args::Attach {
            pids: vec![],
            tids: vec![],
            job_ids: vec![],
            moniker: None,
            url: None,
            buffer_size_mb: None,
            duration: None,
            output: Some(String::from("output_file.pb")),
            ..Default::default()
        };

        let empty_targets = gather_targets(&empty_args);
        assert!(empty_targets.is_err());

        let invalid_args1 = args::Attach {
            pids: vec![1],
            tids: vec![],
            job_ids: vec![],
            moniker: Some(String::from("core/test")),
            buffer_size_mb: Some(8 as u64),
            url: None,
            duration: None,
            output: Some(String::from("output_file.pb")),
            ..Default::default()
        };
        let invalid_args2 = args::Attach {
            pids: vec![],
            tids: vec![1],
            job_ids: vec![],
            moniker: Some(String::from("core/test")),
            url: None,
            buffer_size_mb: Some(8 as u64),
            duration: None,
            output: Some(String::from("output_file.pb")),
            ..Default::default()
        };
        let invalid_args3 = args::Attach {
            pids: vec![],
            tids: vec![],
            job_ids: vec![1],
            moniker: Some(String::from("core/test")),
            buffer_size_mb: Some(8 as u64),
            url: None,
            duration: None,
            output: Some(String::from("output_file.pb")),
            ..Default::default()
        };
        let invalid_targets1 = gather_targets(&invalid_args1);
        assert!(invalid_targets1.is_err());
        let invalid_targets2 = gather_targets(&invalid_args2);
        assert!(invalid_targets2.is_err());
        let invalid_targets3 = gather_targets(&invalid_args3);
        assert!(invalid_targets3.is_err());
    }

    #[test]
    fn test_make_boot_target_config() {
        let target_config_moniker = profiler::TargetConfig::Component(
            profiler::AttachConfig::AttachToComponentMoniker(String::from("core/test")),
        );
        let target_moniker = make_boot_target_config(&target_config_moniker);
        assert!(target_moniker.is_ok());
        if let Ok(profiler::OnBootTargetConfig::Component(
            profiler::OnBootAttachConfig::AttachToComponentMoniker(m),
        )) = target_moniker
        {
            assert_eq!(m, "core/test");
        } else {
            assert!(false, "Expected ComponentMoniker");
        }

        let target_config_url =
            profiler::TargetConfig::Component(profiler::AttachConfig::AttachToComponentUrl(
                String::from("fuchsia-pkg://fuchsia.com/my-pkg#meta/my-component.cm"),
            ));
        let target_url = make_boot_target_config(&target_config_url);
        assert!(target_url.is_ok());
        if let Ok(profiler::OnBootTargetConfig::Component(
            profiler::OnBootAttachConfig::AttachToComponentUrl(u),
        )) = target_url
        {
            assert_eq!(u, "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-component.cm");
        } else {
            assert!(false, "Expected ComponentUrl");
        }

        let target_config_tasks = profiler::TargetConfig::Tasks(vec![profiler::Task::Process(123)]);
        let empty_targets = make_boot_target_config(&target_config_tasks);
        assert!(empty_targets.is_ok());
        if let Ok(profiler::OnBootTargetConfig::Tasks(t)) = empty_targets {
            assert_eq!(t.len(), 1);
            if let profiler::Task::Process(pid) = t[0] {
                assert_eq!(pid, 123);
            } else {
                assert!(false, "Expected Task::Process(123)");
            }
        } else {
            assert!(false, "Expected Tasks");
        }
    }

    #[test]
    fn test_check_background_args_valid() {
        let result = check_background_args(
            None,
            None,
            false,
            true,
            true,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_background_args_with_duration() {
        let result = check_background_args(
            Some(10),
            None,
            false,
            true,
            true,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_invalid_output() {
        let result = check_background_args(
            None,
            Some("custom_profile"),
            false,
            true,
            true,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_print_stats() {
        let result = check_background_args(
            None,
            None,
            true,
            true,
            true,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_no_symbolize() {
        let result = check_background_args(
            None,
            None,
            false,
            false,
            true,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_no_pprof() {
        let result = check_background_args(
            None,
            None,
            false,
            true,
            false,
            args::OutputFormat::Pprof,
            std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_invalid_color() {
        let result = check_background_args(
            None,
            None,
            false,
            true,
            true,
            args::OutputFormat::Pprof,
            !std::io::stdout().is_terminal(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_check_background_args_rejects_non_pprof_output_format() {
        for fmt in [args::OutputFormat::Perfetto, args::OutputFormat::Fxt] {
            let result = check_background_args(
                None,
                None,
                false,
                true,
                true,
                fmt,
                std::io::stdout().is_terminal(),
            );
            assert!(result.is_err(), "expected error for {fmt:?}");
            let msg = result.unwrap_err().to_string();
            assert!(msg.contains("--output-format"), "unexpected error message: {msg}");
            assert!(msg.contains("ffx profiler stop"), "unexpected error message: {msg}");
        }
    }

    #[test]
    fn test_output_format_from_str() {
        use std::str::FromStr;

        assert_eq!(args::OutputFormat::default(), args::OutputFormat::Pprof);
        assert_eq!(args::OutputFormat::from_str("pprof"), Ok(args::OutputFormat::Pprof));
        assert_eq!(args::OutputFormat::from_str("perfetto"), Ok(args::OutputFormat::Perfetto));
        assert_eq!(args::OutputFormat::from_str("fxt"), Ok(args::OutputFormat::Fxt));

        for invalid in ["", "json", "perfetto-fxt", "PERFETTO", "perfetto_fxt"] {
            let err = args::OutputFormat::from_str(invalid).unwrap_err();
            assert!(
                err.contains("valid options are 'pprof', 'perfetto', 'fxt'"),
                "unexpected error for '{invalid}': {err}"
            );
        }
    }

    #[test]
    fn test_create_session_opts_extension() {
        // Test that default is used if output is None
        let opts = create_session_opts(
            true,
            None,
            false,
            None,
            None,
            true,
            args::OutputFormat::Pprof,
            true,
        );
        assert_eq!(opts.output, "profile.pb");
        assert_eq!(opts.output_format, args::OutputFormat::Pprof);

        let opts = create_session_opts(
            true,
            None,
            false,
            None,
            None,
            true,
            args::OutputFormat::Perfetto,
            true,
        );
        assert_eq!(opts.output, "profile.pftrace");
        assert_eq!(opts.output_format, args::OutputFormat::Perfetto);

        let opts =
            create_session_opts(true, None, false, None, None, true, args::OutputFormat::Fxt, true);
        assert_eq!(opts.output, "profile.fxt");
        assert_eq!(opts.output_format, args::OutputFormat::Fxt);

        // When symbolize is false, --output-format is ignored and default is profile.fxt
        for fmt in
            [args::OutputFormat::Pprof, args::OutputFormat::Perfetto, args::OutputFormat::Fxt]
        {
            let opts = create_session_opts(false, None, false, None, None, true, fmt, true);
            assert_eq!(opts.output, "profile.fxt");
        }

        // Test that output is literal if Some is provided
        for fmt in
            [args::OutputFormat::Pprof, args::OutputFormat::Perfetto, args::OutputFormat::Fxt]
        {
            let opts = create_session_opts(
                true,
                None,
                false,
                Some("profile".to_string()),
                None,
                true,
                fmt,
                true,
            );
            assert_eq!(opts.output, "profile");

            let opts = create_session_opts(
                true,
                None,
                false,
                Some("custom/out.file".to_string()),
                None,
                true,
                fmt,
                true,
            );
            assert_eq!(opts.output, "custom/out.file");
        }
    }

    #[test]
    fn test_retained_raw_fxt_path() {
        use std::path::{Path, PathBuf};

        assert_eq!(
            retained_raw_fxt_path(Path::new("/tmp/out/profile.fxt")),
            PathBuf::from("/tmp/out/profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("profile.fxt")),
            PathBuf::from("profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("/tmp/out/profile.perfetto.fxt")),
            PathBuf::from("/tmp/out/profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("profile.perfetto.fxt")),
            PathBuf::from("profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("/tmp/out/profile.pftrace")),
            PathBuf::from("/tmp/out/profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("profile.pftrace")),
            PathBuf::from("profile.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("/tmp/out/custom_name")),
            PathBuf::from("/tmp/out/custom_name.raw.fxt")
        );
        assert_eq!(
            retained_raw_fxt_path(Path::new("/tmp/out/profile.raw.fxt")),
            PathBuf::from("/tmp/out/profile.raw.raw.fxt")
        );
    }

    /// Builds a minimal raw profiler capture: an init record, one module + mapping for PID 1104
    /// and two 2-frame backtrace samples for TID 2616.
    fn build_minimal_profiler_fxt_bytes() -> Vec<u8> {
        use fxt::fxt_builder::FxtBuilder;
        use fxt::init::InitHeader;
        use fxt::profiler::{BacktraceRecordHeader, MmapRecordHeader, ModuleRecordHeader};

        const TICKS_PER_SECOND: u64 = 38_400_000;
        const SAMPLE_TICKS_1: u64 = 9_903_928_169_907;
        const SAMPLE_TICKS_2: u64 = 9_903_928_200_000;
        const PID: u64 = 1104;
        const TID: u64 = 2616;
        const MODULE_NAME: &[u8] = b"libtrace-engine.so";
        const BUILD_ID: [u8; 20] = [
            0x33, 0x3e, 0x89, 0xf0, 0xc1, 0x75, 0x00, 0x0c, 0xee, 0x9b, 0x7e, 0x20, 0x1f, 0xed,
            0xcd, 0x6f, 0x9b, 0x4b, 0xa8, 0xae,
        ];
        // Profiler record sub-types. `empty()` leaves thread_ref at 0, i.e. inline pid/tid.
        const MODULE_SUB_TYPE: u8 = 0;
        const MMAP_SUB_TYPE: u8 = 1;
        const BACKTRACE_SUB_TYPE: u8 = 2;
        const READ_EXECUTE: u8 = 5;

        let mut module_header = ModuleRecordHeader::empty();
        module_header.set_profiler_sub_type(MODULE_SUB_TYPE);
        module_header.set_module_id(0);
        module_header.set_name_length(MODULE_NAME.len() as u8);
        module_header.set_build_id_length(BUILD_ID.len() as u8);

        let mut mmap_header = MmapRecordHeader::empty();
        mmap_header.set_profiler_sub_type(MMAP_SUB_TYPE);
        mmap_header.set_module_id(0);
        mmap_header.set_flags(READ_EXECUTE);

        let mut backtrace_header = BacktraceRecordHeader::empty();
        backtrace_header.set_profiler_sub_type(BACKTRACE_SUB_TYPE);
        backtrace_header.set_num_records(2);

        let backtrace = |ticks: u64, frames: [u64; 2]| {
            FxtBuilder::new(backtrace_header)
                .atom(ticks.to_le_bytes())
                .atom(PID.to_le_bytes())
                .atom(TID.to_le_bytes())
                .atom(frames[0].to_le_bytes())
                .atom(frames[1].to_le_bytes())
                .build()
        };

        let records = [
            FxtBuilder::new(InitHeader::empty()).atom(TICKS_PER_SECOND.to_le_bytes()).build(),
            FxtBuilder::new(module_header)
                .atom(0u64.to_le_bytes()) // timestamp
                .atom(PID.to_le_bytes())
                .atom(0u64.to_le_bytes()) // tid
                .atom(MODULE_NAME)
                .atom(BUILD_ID)
                .build(),
            FxtBuilder::new(mmap_header)
                .atom(0u64.to_le_bytes()) // timestamp
                .atom(PID.to_le_bytes())
                .atom(0u64.to_le_bytes()) // tid
                .atom(0xc93639c000u64.to_le_bytes()) // start_addr
                .atom(0xd000u64.to_le_bytes()) // size
                .atom(0x6000u64.to_le_bytes()) // vaddr
                .build(),
            backtrace(SAMPLE_TICKS_1, [0xc93639c100, 0xc93639c200]),
            backtrace(SAMPLE_TICKS_2, [0xc93639c150, 0xc93639c200]),
        ];

        let mut bytes = fxt::FXT_MAGIC_BYTES.to_vec();
        bytes.extend(records.iter().flatten().flat_map(|word| word.to_le_bytes()));
        assert_eq!(bytes.len(), 8 + 8 * (2 + 10 + 7 + 6 + 6));
        bytes
    }

    #[fuchsia::test]
    async fn test_symbolize_perfetto_and_perfetto_fxt_offline() {
        use ffx_writer::TestBuffers;
        use fxt::TraceRecord;
        use fxt::blob::BlobType;
        use fxt::session::parse_full_session;
        use perfetto_trace_rust_proto::perfetto::protos;
        use prost::Message;

        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let input_path = tmp_dir.path().join("input_raw.fxt");
        let pftrace_path = tmp_dir.path().join("out.pftrace");
        let fxt_path = tmp_dir.path().join("out.fxt");

        let raw_bytes = build_minimal_profiler_fxt_bytes();
        std::fs::write(&input_path, &raw_bytes).expect("write raw fxt");
        let env = ffx_config::test_env().build().expect("test env");

        // 1. Run `ProfilerSubCommand::Symbolize` with `OutputFormat::Perfetto` (no device needed).
        let tool_pftrace = ProfilerTool {
            controller: fho::Deferred::from_output(Err(fho::Error::Unexpected(anyhow::anyhow!(
                "controller should not be accessed for offline symbolize"
            )))),
            session_manager: fho::Deferred::from_output(Err(fho::Error::Unexpected(
                anyhow::anyhow!("session_manager should not be accessed for offline symbolize"),
            ))),
            cmd: ProfilerCommand {
                sub_cmd: ProfilerSubCommand::Symbolize(args::Symbolize {
                    input: input_path.clone(),
                    output: pftrace_path.clone(),
                    pprof_conversion: true,
                    output_format: args::OutputFormat::Perfetto,
                }),
            },
            context: env.context.clone(),
        };
        let buffers = TestBuffers::default();
        let writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);
        tool_pftrace.profiler(writer).await.expect("symbolize perfetto should succeed");

        let pftrace_bytes = std::fs::read(&pftrace_path).expect("read pftrace");
        assert!(!pftrace_bytes.is_empty());
        let decoded = protos::Trace::decode(pftrace_bytes.as_slice()).expect("decode Trace proto");
        let perf_samples: Vec<_> = decoded
            .packet
            .iter()
            .filter_map(|p| match &p.data {
                Some(protos::trace_packet::Data::PerfSample(s)) => Some((p.timestamp, s)),
                _ => None,
            })
            .collect();
        assert_eq!(perf_samples.len(), 2);
        assert_eq!(perf_samples[0].0, Some(257_914_796_091_328));
        assert_eq!(perf_samples[0].1.pid, Some(1104));
        assert_eq!(perf_samples[0].1.tid, Some(2616));

        // 2. Run `ProfilerSubCommand::Symbolize` with `OutputFormat::Fxt`.
        let tool_fxt = ProfilerTool {
            controller: fho::Deferred::from_output(Err(fho::Error::Unexpected(anyhow::anyhow!(
                "controller should not be accessed for offline symbolize"
            )))),
            session_manager: fho::Deferred::from_output(Err(fho::Error::Unexpected(
                anyhow::anyhow!("session_manager should not be accessed for offline symbolize"),
            ))),
            cmd: ProfilerCommand {
                sub_cmd: ProfilerSubCommand::Symbolize(args::Symbolize {
                    input: input_path.clone(),
                    output: fxt_path.clone(),
                    pprof_conversion: true,
                    output_format: args::OutputFormat::Fxt,
                }),
            },
            context: env.context.clone(),
        };
        let buffers = TestBuffers::default();
        let writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);
        tool_fxt.profiler(writer).await.expect("symbolize fxt should succeed");

        let fxt_bytes = std::fs::read(&fxt_path).expect("read out.fxt");
        let (records, warnings) =
            parse_full_session(&fxt_bytes).expect("parse_full_session on out.fxt");
        assert!(warnings.is_empty(), "unexpected FXT warnings: {warnings:?}");

        let mut extracted_blob_bytes = Vec::new();
        for rec in records {
            if let TraceRecord::Blob(blob) = rec {
                assert_eq!(blob.ty, BlobType::Perfetto);
                assert_eq!(blob.name.as_str(), "ffx_profiler");
                extracted_blob_bytes.extend_from_slice(&blob.bytes);
            }
        }
        assert_eq!(extracted_blob_bytes, pftrace_bytes);
    }

    #[fuchsia::test]
    async fn test_finalize_profile_session_retains_raw_fxt_and_writes_perfetto_containers() {
        use fdomain_fuchsia_cpu_profiler::{SessionRequest, SessionResult};
        use ffx_writer::TestBuffers;
        use fxt::TraceRecord;
        use fxt::blob::BlobType;
        use fxt::session::parse_full_session;
        use perfetto_trace_rust_proto::perfetto::protos;
        use prost::Message;
        use target_holders::fake_proxy;

        let client = fdomain_local::local_client_empty();
        let proxy: profiler::SessionProxy =
            fake_proxy(std::sync::Arc::clone(&client), move |req| match req {
                SessionRequest::Stop { responder } => {
                    responder
                        .send(&SessionResult { samples_collected: Some(2), ..Default::default() })
                        .unwrap();
                }
                SessionRequest::Reset { responder } => {
                    responder.send().unwrap();
                }
                _ => panic!("Unexpected request: {:?}", req),
            });
        let session_proxy: ProfilerSessionManagerProxy = proxy.into();

        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let raw_bytes = build_minimal_profiler_fxt_bytes();
        let unsymbolized_path = tmp_dir.path().join("unsymbolized.txt");
        std::fs::write(&unsymbolized_path, &raw_bytes).expect("write unsymbolized");

        // 1. Test `OutputFormat::Fxt` -> writes `profile.fxt` and retains `profile.raw.fxt`.
        let out_fxt = tmp_dir.path().join("profile.fxt");
        let expected_raw_fxt = tmp_dir.path().join("profile.raw.fxt");
        let opts_fxt = SessionOpts {
            symbolize: true,
            buffer_size_mb: None,
            print_stats: false,
            pprof_conversion: true,
            output_format: args::OutputFormat::Fxt,
            output: out_fxt.to_str().unwrap().to_string(),
            duration: None,
            color_output: false,
        };
        let buffers = TestBuffers::default();
        let mut writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);
        let env = ffx_config::test_env().build().expect("test env");

        finalize_profile_session(
            &session_proxy,
            &mut writer,
            &opts_fxt,
            None,
            &unsymbolized_path,
            &env.context,
            false,
        )
        .await
        .expect("finalize_profile_session Fxt should succeed");

        assert!(expected_raw_fxt.exists(), "retained raw FXT must exist at {expected_raw_fxt:?}");
        assert_eq!(
            std::fs::read(&expected_raw_fxt).expect("read retained raw FXT"),
            raw_bytes,
            "retained raw FXT must be byte-identical to raw unsymbolized FXT"
        );

        let fxt_out_bytes = std::fs::read(&out_fxt).expect("read profile.fxt");
        let (records, warnings) =
            parse_full_session(&fxt_out_bytes).expect("parse_full_session on profile.fxt");
        assert!(warnings.is_empty(), "unexpected FXT warnings: {warnings:?}");
        let blob_payload: Vec<u8> = records
            .into_iter()
            .filter_map(|r| match r {
                TraceRecord::Blob(b) => {
                    assert_eq!(b.ty, BlobType::Perfetto);
                    Some(b.bytes)
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert!(!blob_payload.is_empty());

        // 2. Test `OutputFormat::Perfetto` -> writes `capture.pftrace` and retains `capture.raw.fxt`.
        let out_pftrace = tmp_dir.path().join("capture.pftrace");
        let expected_capture_raw = tmp_dir.path().join("capture.raw.fxt");
        let opts_pf = SessionOpts {
            symbolize: true,
            buffer_size_mb: None,
            print_stats: false,
            pprof_conversion: true,
            output_format: args::OutputFormat::Perfetto,
            output: out_pftrace.to_str().unwrap().to_string(),
            duration: None,
            color_output: false,
        };
        finalize_profile_session(
            &session_proxy,
            &mut writer,
            &opts_pf,
            None,
            &unsymbolized_path,
            &env.context,
            false,
        )
        .await
        .expect("finalize_profile_session Perfetto should succeed");

        assert!(expected_capture_raw.exists());
        assert_eq!(std::fs::read(&expected_capture_raw).unwrap(), raw_bytes);
        let pftrace_bytes = std::fs::read(&out_pftrace).expect("read capture.pftrace");
        assert_eq!(pftrace_bytes, blob_payload);
        let trace = protos::Trace::decode(pftrace_bytes.as_slice()).expect("decode pftrace");
        assert!(!trace.packet.is_empty());
    }

    #[fuchsia::test]
    async fn test_stop_no_active_session() {
        use fdomain_fuchsia_cpu_profiler::{ManagerError, SessionManagerRequest};
        use ffx_writer::TestBuffers;
        use target_holders::fake_proxy;
        let client = fdomain_local::local_client_empty();

        let session_manager = fake_proxy(std::sync::Arc::clone(&client), move |req| match req {
            SessionManagerRequest::StopSession { payload: _, responder } => {
                responder.send(Err(ManagerError::NoSuchTask)).unwrap();
            }
            SessionManagerRequest::AbortSession { payload: _, responder } => {
                responder.send(Err(ManagerError::NoSuchTask)).unwrap();
            }
            _ => panic!("Unexpected request: {:?}", req),
        });

        let env = ffx_config::test_env().build().expect("test env");
        let tool = ProfilerTool {
            controller: fho::Deferred::from_output(Err(fho::Error::Unexpected(anyhow::anyhow!(
                "controller should not be accessed"
            )))),
            session_manager: fho::Deferred::from_output(Ok(session_manager)),
            cmd: ProfilerCommand {
                sub_cmd: ProfilerSubCommand::Stop(args::Stop {
                    output: None,
                    abort: false,
                    symbolize: false,
                    pprof_conversion: false,
                    output_format: args::OutputFormat::Pprof,
                    color_output: false,
                    print_stats: false,
                }),
            },
            context: env.context.clone(),
        };

        let buffers = TestBuffers::default();
        let writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);

        let result = tool.profiler(writer).await;

        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Failed to finalize session"));
        assert!(err_str.contains("NoSuchTask"));
    }

    #[fuchsia::test]
    async fn test_attach_and_launch_background_rejects_non_pprof_output_format() {
        use ffx_writer::TestBuffers;
        use target_holders::fake_proxy;

        let client = fdomain_local::local_client_empty();
        let env = ffx_config::test_env().build().expect("test env");

        for fmt in [args::OutputFormat::Perfetto, args::OutputFormat::Fxt] {
            let attach_session_manager: profiler::SessionManagerProxy =
                fake_proxy(std::sync::Arc::clone(&client), |req| {
                    panic!("no request expected before argument validation: {req:?}")
                });
            let attach_tool = ProfilerTool {
                controller: fho::Deferred::from_output(Err(fho::Error::Unexpected(
                    anyhow::anyhow!("controller must not be accessed when background args invalid"),
                ))),
                session_manager: fho::Deferred::from_output(Ok(attach_session_manager)),
                cmd: ProfilerCommand {
                    sub_cmd: ProfilerSubCommand::Attach(args::Attach {
                        pids: vec![123],
                        background: true,
                        symbolize: true,
                        pprof_conversion: true,
                        output_format: fmt,
                        color_output: std::io::stdout().is_terminal(),
                        ..Default::default()
                    }),
                },
                context: env.context.clone(),
            };
            let buffers = TestBuffers::default();
            let writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);
            let err = attach_tool.profiler(writer).await.unwrap_err().to_string();
            assert!(
                err.contains("--output-format") && err.contains("ffx profiler stop"),
                "unexpected attach error for {fmt:?}: {err}"
            );

            let launch_session_manager: profiler::SessionManagerProxy =
                fake_proxy(std::sync::Arc::clone(&client), |req| {
                    panic!("no request expected before argument validation: {req:?}")
                });
            let launch_tool = ProfilerTool {
                controller: fho::Deferred::from_output(Err(fho::Error::Unexpected(
                    anyhow::anyhow!("controller must not be accessed when background args invalid"),
                ))),
                session_manager: fho::Deferred::from_output(Ok(launch_session_manager)),
                cmd: ProfilerCommand {
                    sub_cmd: ProfilerSubCommand::Launch(args::Launch {
                        url: "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm".to_string(),
                        background: true,
                        symbolize: true,
                        pprof_conversion: true,
                        output_format: fmt,
                        color_output: std::io::stdout().is_terminal(),
                        ..Default::default()
                    }),
                },
                context: env.context.clone(),
            };
            let buffers = TestBuffers::default();
            let writer = <ProfilerTool as FfxMain>::Writer::new_test(None, &buffers);
            let err = launch_tool.profiler(writer).await.unwrap_err().to_string();
            assert!(
                err.contains("--output-format") && err.contains("ffx profiler stop"),
                "unexpected launch error for {fmt:?}: {err}"
            );
        }
    }
}
