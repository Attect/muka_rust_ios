use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Args, CommandFactory, Parser, Subcommand};
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, ConnectionPreference,
    DebugAppInfo, DebugProxySpec, DproxyMode, DproxySpec, ErrorCode, ForwardSpec,
    LocationSimulationSpec, OsTraceLevel, OsTraceOptions, OutputOptions, ProtocolError, Request,
    ResponsePayload, RuntimeState, ScreenshotStreamSpec, StreamFormat, StreamTaskSpec, TaskKind,
    TaskSelector, TunnelMode, TunnelPairSpec, TunnelSpec, TunnelTransport, UiAutomationDriver,
    UiRunnerSpec, UiRunnerTarget, UiStreamFormat, UiStreamSpec, WdaSpec, WebInspectorCdpSpec,
    WebInspectorJsShellSpec, XCTestEnvironmentValue, XCTestSpec, XCTestTargetSpec,
};
use rust_ios_server::{
    ClientError, ServerConfig, ServerPaths, connect_existing, ensure_server, run_default_server,
};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use uuid::Uuid;

mod gpx;
mod xctestrun;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LEGACY_HELP: &str = r#"
go-ios 兼容命令入口（迁移状态见 docs/feature-matrix.md）：
  activate apps assistivetouch ax batterycheck batteryregistry crash date debug
  devicename devicestate devmode diagnostics diskspace dproxy erase file fsync
  httpproxy image info install instruments ip kill lang launch lockdown
  memlimitoff mobilegestalt pair pcap prepare profile ps readpair reboot resetax
  resetlocation rsd runtest runwda runxctest screenshot sign setlocation
  setlocationgpx shutdown set-wallpaper get-app-icon get-wallpaper get-icon-layout
  set-icon-layout syslog ostrace sysmontap timeformat tunnel ui uninstall
  webinspector voiceover wifi zoom

持续任务变化：tunnel、forward、syslog/pcap/stream 等由常驻 server 持有；
普通 client 不再为它们生成单独长期进程。
"#;

#[derive(Debug, Parser)]
#[command(
    name = "ios",
    about = "Rust 版 go-ios：ADB 风格的单一 client/server iOS 工具",
    disable_version_flag = true,
    disable_help_subcommand = true,
    after_long_help = LEGACY_HELP,
    allow_external_subcommands = true
)]
struct Cli {
    #[arg(long = "version", action = clap::ArgAction::SetTrue, global = true)]
    version_flag: bool,
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
    #[arg(short = 't', long, global = true)]
    trace: bool,
    #[arg(long, global = true)]
    nojson: bool,
    #[arg(long, global = true)]
    pretty: bool,
    #[arg(long, env = "GO_IOS_UDID", global = true)]
    udid: Option<String>,
    #[arg(long, hide = true, global = true)]
    server_addr: Option<SocketAddr>,
    #[arg(long, hide = true, global = true)]
    server_home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<TopCommand>,
}

#[derive(Debug, Subcommand)]
enum TopCommand {
    Version,
    #[command(name = "start-server")]
    StartServer,
    #[command(name = "kill-server")]
    KillServer,
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    List {
        #[arg(long)]
        details: bool,
    },
    Listen,
    #[command(name = "readpair")]
    ReadPair,
    Buid,
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Forward(ForwardArgs),
    Syslog(SyslogArgs),
    Ostrace(OsTraceArgs),
    Pcap(PcapArgs),
    Tunnel {
        #[command(subcommand)]
        command: TunnelCommand,
    },
    Help {
        #[arg(value_name = "COMMAND", num_args = 0..)]
        command: Vec<String>,
    },
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    Status,
    #[command(hide = true)]
    Nodaemon {
        #[arg(long, hide = true)]
        daemon_child: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    List {
        #[arg(long)]
        kind: Option<String>,
    },
    Show {
        task_id: Uuid,
    },
    Stop {
        selector: String,
    },
    Restart {
        task_id: Uuid,
    },
    Output {
        task_id: Uuid,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 65_536)]
        limit: u32,
        #[arg(long)]
        raw: bool,
    },
}

#[derive(Debug, Args)]
struct ForwardArgs {
    /// 兼容格式：<hostPort> <targetPort>；扩展格式：list/stop/clear。
    #[arg(value_name = "ARG", num_args = 0..)]
    values: Vec<String>,
    #[arg(long = "port", action = clap::ArgAction::Append)]
    mappings: Vec<String>,
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    #[arg(long)]
    network: bool,
    #[arg(long)]
    usb: bool,
}

#[derive(Debug, Args)]
struct SyslogArgs {
    #[arg(long)]
    parse: bool,
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct OsTraceArgs {
    #[arg(long)]
    pid: Option<u32>,
    #[arg(long)]
    process: Option<String>,
    #[arg(long)]
    level: Option<String>,
    #[arg(long)]
    subsystem: Option<String>,
    #[arg(long = "match")]
    match_filter: Option<String>,
    #[arg(long)]
    exclude: Option<String>,
    #[arg(long)]
    follow: bool,
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct PcapArgs {
    #[arg(long)]
    pid: Option<u32>,
    #[arg(long)]
    process: Option<String>,
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum TunnelCommand {
    Start {
        #[arg(long)]
        userspace: bool,
        #[arg(long)]
        remote_pairing: bool,
        #[arg(long, default_value = "default")]
        pair_record_path: String,
        #[arg(long, default_value = "127.0.0.1")]
        userspace_bind: String,
        #[arg(long, default_value_t = 0)]
        userspace_port: u16,
        #[arg(long, value_name = "NAME")]
        interface_name: Option<String>,
        #[arg(long, value_name = "FILE")]
        wintun_path: Option<PathBuf>,
    },
    Pair {
        #[arg(long)]
        manual: bool,
        #[arg(long, default_value = "default")]
        pair_record_path: String,
    },
    #[command(name = "ls")]
    List,
    Stop,
    Refresh,
    Stopagent,
}

fn normalized_cli_args(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut arguments = arguments.into_iter();
    let executable = arguments.next().unwrap_or_else(|| OsString::from("ios"));
    let values: Vec<OsString> = arguments.collect();
    let mut globals = Vec::new();
    let mut command = Vec::new();
    let mut index = 0;
    while index < values.len() {
        let text = values[index].to_string_lossy();
        let flag_only = matches!(
            text.as_ref(),
            "--version" | "-v" | "--verbose" | "-t" | "--trace" | "--nojson" | "--pretty"
        );
        let value_option = matches!(text.as_ref(), "--udid" | "--server-addr" | "--server-home");
        let inline_option = text.starts_with("--udid=")
            || text.starts_with("--server-addr=")
            || text.starts_with("--server-home=");
        if flag_only || inline_option {
            globals.push(values[index].clone());
        } else if value_option {
            globals.push(values[index].clone());
            if let Some(value) = values.get(index + 1) {
                globals.push(value.clone());
                index += 1;
            }
        } else {
            command.push(values[index].clone());
        }
        index += 1;
    }
    let mut normalized = Vec::with_capacity(1 + globals.len() + command.len());
    normalized.push(executable);
    normalized.extend(globals);
    normalized.extend(command);
    normalized
}

fn external_help_request(arguments: &[OsString]) -> Option<String> {
    let help_index = arguments
        .iter()
        .position(|value| matches!(value.to_string_lossy().as_ref(), "-h" | "--help"))?;
    let mut index = 1;
    let mut command = None;
    while index < help_index {
        let value = arguments[index].to_string_lossy();
        if matches!(value.as_ref(), "--udid" | "--server-addr" | "--server-home") {
            index += 2;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        command = Some(value.into_owned());
        break;
    }
    let command = command?;
    if matches!(
        command.as_str(),
        "version"
            | "start-server"
            | "kill-server"
            | "server"
            | "list"
            | "listen"
            | "readpair"
            | "buid"
            | "task"
            | "forward"
            | "syslog"
            | "ostrace"
            | "pcap"
            | "tunnel"
            | "help"
    ) {
        None
    } else {
        Some(command)
    }
}

fn print_legacy_command_help(command: &str) {
    let (usage, summary) = match command {
        "apps" => (
            "ios apps [--list] [--system] [--all] [options]",
            "List installed applications.",
        ),
        "ax" => (
            "ios ax [--font=<fontSize>] [options]\n  ios ax audit [--timeout=<seconds>] [options]",
            "Start the server-owned Accessibility inspector or run an audit.",
        ),
        "ax audit" => (
            "ios ax audit [--timeout=<seconds>] [options]",
            "Run the device Accessibility audit.",
        ),
        "batterycheck" => ("ios batterycheck [options]", "Read battery diagnostics."),
        "batteryregistry" => (
            "ios batteryregistry [options]",
            "Read battery registry metrics.",
        ),
        "date" => ("ios date [options]", "Print the device date."),
        "devicename" => ("ios devicename [options]", "Print the device name."),
        "devmode" => (
            "ios devmode (enable|get|reveal) [--enable-post-restart] [options]",
            "Manage Developer Mode.",
        ),
        "debug" => (
            "ios debug [--stop-at-entry] <app_path> [options]\n  ios debug proxy [--bind=<host>] [--port=<port>] [options]",
            "Start an app debug session or a server-owned raw debug proxy.",
        ),
        "diagnostics" => (
            "ios diagnostics list [options]",
            "Read all diagnostics_relay data.",
        ),
        "diskspace" => ("ios diskspace [options]", "Read AFC filesystem usage."),
        "erase" => (
            "ios erase --force --udid=<UDID> [options]",
            "Irreversibly erase all apps and settings from the explicitly selected device.",
        ),
        "file" => (
            "ios file (ls|pull|push) [options]",
            "Access AFC or an app container.",
        ),
        "get-icon-layout" => (
            "ios get-icon-layout [--output=<file>] [options]",
            "Read the SpringBoard icon layout.",
        ),
        "get-app-icon" => (
            "ios get-app-icon <bundleID> [--output=<file>] [options]",
            "Save an application's SpringBoard icon as PNG.",
        ),
        "get-wallpaper" => (
            "ios get-wallpaper [--output=<file>] [options]",
            "Save the home screen wallpaper preview.",
        ),
        "image" => (
            "ios image (auto [--basedir=<dir>] | list | mount --path=<image> | unmount) [options]",
            "Manage Developer Disk Images.",
        ),
        "info" => (
            "ios info [display | lockdown] [options]",
            "Dump Lockdown or CoreDevice display information.",
        ),
        "install" => (
            "ios install --path=<app-or-ipa> [options]",
            "Install an application package.",
        ),
        "kill" => (
            "ios kill [<bundleID>] [--pid=<pid>] [--process=<name>] [options]",
            "Kill a running process.",
        ),
        "launch" => (
            "ios launch <bundleID> [--arg=<arg>]... [--env=<key=value>]... [--kill-existing] [--wait] [options]",
            "Launch an application.",
        ),
        "lockdown" => (
            "ios lockdown get [<key>] [--domain=<domain>] [options]",
            "Query Lockdown values.",
        ),
        "memlimitoff" => (
            "ios memlimitoff --process=<name> [options]",
            "Disable a process memory limit.",
        ),
        "mobilegestalt" => (
            "ios mobilegestalt <key>... [--plist] [options]",
            "Query MobileGestalt keys.",
        ),
        "pair" => (
            "ios pair [--name=<host-name>] [options]\n  ios pair --p12file=<organization.p12> [--password=<password>] [options]",
            "Pair through the trust dialog or a supervised organization identity.",
        ),
        "ps" => ("ios ps [--apps] [options]", "List running processes."),
        "reboot" => ("ios reboot [options]", "Reboot a device."),
        "resetax" => (
            "ios resetax [--timeout=<seconds>] [options]",
            "Reset device Accessibility settings to defaults.",
        ),
        "resetlocation" => ("ios resetlocation [options]", "Clear simulated location."),
        "rsd" | "rsd ls" => (
            "ios rsd ls [options]",
            "List services advertised by the active server-owned tunnel.",
        ),
        "screenshot" => (
            "ios screenshot [--output=<file>] [options]",
            "Capture one screenshot.",
        ),
        "set-icon-layout" => (
            "ios set-icon-layout <layoutFile> [options]",
            "Apply a SpringBoard icon layout.",
        ),
        "set-wallpaper" => (
            "ios set-wallpaper <imagePath> --p12file=<identity.p12> [--password=<password>] [--screen=lock|home|both] [options]",
            "Set a supervised device wallpaper from a JPEG or PNG file.",
        ),
        "setlocation" => (
            "ios setlocation --lat=<latitude> --lon=<longitude> [options]",
            "Set a simulated location.",
        ),
        "setlocationgpx" => (
            "ios setlocationgpx --gpxfilepath=<file> [options]",
            "Run a GPX route as a server-owned finite task.",
        ),
        "shutdown" => ("ios shutdown [options]", "Shut down a device."),
        "ui" => (
            "ios ui download [wda|devicekit|all] [--output=<dir>] [options]\n  ios ui install (wda|devicekit) --p12file=<p12> --profile=<mobileprovision> [--path=<artifact>] [--output=<signed>] [--bundleid=<id>] [options]\n  ios ui (status|api|raw|tap|swipe|longpress|type|button|screenshot|source|size|orientation|app|stream) [options]\n  ios ui run (wda|devicekit) [--bundleid=<id>] [--test-runner-bundleid=<id>] [--xctest-config=<name>] [--host-port=<port>] [--log-output=<file>] [options]",
            "Download, install, or call a WDA or DeviceKit UI automation backend.",
        ),
        "sign" => (
            "ios sign app --path=<app-or-ipa> --p12file=<p12> --profile=<mobileprovision> [--p12password=<password>] [--output=<signed>] [--bundleid=<id>] [--install] [options]\n  ios sign certificate appstoreconnect --asc-key-id=<id> --asc-issuer-id=<id> --asc-private-key=<p8> [--p12-output=<p12>] [--revoke-existing] [options]\n  ios sign provision appstoreconnect --bundleid=<id> --asc-key-id=<id> --asc-issuer-id=<id> --asc-private-key=<p8> [--profile-output=<profile>] [--certificate-id=<id>] [options]",
            "Create App Store Connect signing assets or re-sign an Apple app bundle/IPA.",
        ),
        "uninstall" => (
            "ios uninstall <bundleID> [options]",
            "Uninstall an application.",
        ),
        "webinspector" => (
            "ios webinspector list [--timeout=<seconds>] [options]\n  ios webinspector launch <url> [--bundle-id=<bundleID>] [--timeout=<seconds>] [options]\n  ios webinspector eval <pageID> <expression> [--timeout=<seconds>] [--console-enable] [options]\n  ios webinspector cdp [--host=<host>] [--port=<port>] [--timeout=<seconds>] [options]\n  ios webinspector js-shell [<url>] [--bundle-id=<bundleID>] [--open-safari] [--timeout=<seconds>] [options]",
            "List inspectable pages, launch Remote Automation, evaluate JavaScript, or start a server-owned CDP endpoint.",
        ),
        _ => (
            "ios <command> [options]",
            "go-ios compatible command entry.",
        ),
    };
    println!(
        "{summary}\n\nUsage:\n  {}",
        usage.replace("<command>", command)
    );
    println!("\nGlobal options: --udid, --nojson, --pretty, --verbose, --trace");
    println!("Migration details: docs/feature-matrix.md");
}

#[tokio::main]
async fn main() -> ExitCode {
    let raw_arguments: Vec<OsString> = std::env::args_os().collect();
    if let Some(command) = external_help_request(&raw_arguments) {
        print_legacy_command_help(&command);
        return ExitCode::SUCCESS;
    }
    let cli = Cli::parse_from(normalized_cli_args(raw_arguments));
    let output = OutputOptions {
        no_json: cli.nojson,
        pretty: cli.pretty,
        verbose: cli.verbose,
        trace: cli.trace,
    };
    match run(cli, &output).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error, &output);
            ExitCode::from(exit_code(&error))
        }
    }
}

async fn run(mut cli: Cli, output: &OutputOptions) -> anyhow::Result<()> {
    if cli.version_flag || matches!(cli.command, Some(TopCommand::Version)) {
        return print_value(&json!({ "version": VERSION }), output, Some(VERSION));
    }

    let Some(command) = cli.command.take() else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    if let TopCommand::Help { command } = &command {
        if command.is_empty() {
            Cli::command().print_long_help()?;
            println!();
        } else {
            print_legacy_command_help(&command.join(" "));
        }
        return Ok(());
    }

    let config = server_config(&cli)?;
    match command {
        TopCommand::Version | TopCommand::Help { .. } => unreachable!(),
        TopCommand::Server {
            command: ServerCommand::Nodaemon { daemon_child },
        } => {
            let _logging_guard =
                init_server_logging(cli.verbose, cli.trace, daemon_child, &config.paths.log);
            run_default_server(config).await?;
            Ok(())
        }
        TopCommand::StartServer => {
            let ensured = ensure_server(&config).await?;
            print_value(
                &json!({
                    "status": if ensured.started { "started" } else { "already_running" },
                    "pid": ensured.client.hello.server_pid,
                    "instanceId": ensured.client.hello.instance_id,
                    "version": ensured.client.hello.server_version,
                }),
                output,
                Some(if ensured.started {
                    "server started"
                } else {
                    "server already running"
                }),
            )
        }
        TopCommand::KillServer => {
            let mut client = connect_existing(&config).await?;
            let payload = client.request(Request::Shutdown).await?;
            print_payload(payload, output, false)
        }
        TopCommand::Server {
            command: ServerCommand::Status,
        } => {
            let mut client = connect_existing(&config).await?;
            let payload = client.request(Request::ServerStatus).await?;
            print_payload(payload, output, false)
        }
        TopCommand::List { details } => {
            let payload = request_auto(&config, Request::ListDevices { details }).await?;
            print_payload(payload, output, details)
        }
        TopCommand::Listen => listen_devices(&config, output).await,
        TopCommand::ReadPair => {
            let udid = cli
                .udid
                .clone()
                .ok_or_else(|| anyhow!("readpair requires --udid or GO_IOS_UDID"))?;
            let payload = request_auto(&config, Request::ReadPairRecord { udid }).await?;
            print_payload(payload, output, true)
        }
        TopCommand::Buid => {
            let payload = request_auto(&config, Request::ReadBuid).await?;
            print_payload(payload, output, false)
        }
        TopCommand::Task { command } => match command {
            TaskCommand::Output {
                task_id,
                offset,
                limit,
                raw,
            } => {
                let payload = request_auto(
                    &config,
                    Request::TaskOutput {
                        task_id,
                        offset,
                        limit,
                    },
                )
                .await?;
                if raw {
                    print_task_output_raw(payload)
                } else {
                    print_payload(payload, output, true)
                }
            }
            command => {
                let request = match command {
                    TaskCommand::List { kind } => Request::TaskList {
                        kind: kind.map(parse_task_kind).transpose()?,
                    },
                    TaskCommand::Show { task_id } => Request::TaskShow { task_id },
                    TaskCommand::Stop { selector } => Request::TaskStop {
                        selector: parse_task_selector(&selector)?,
                    },
                    TaskCommand::Restart { task_id } => Request::TaskRestart { task_id },
                    TaskCommand::Output { .. } => unreachable!(),
                };
                print_payload(request_auto(&config, request).await?, output, true)
            }
        },
        TopCommand::Forward(arguments) => {
            let request = forward_request(arguments, cli.udid.clone())?;
            print_payload(request_auto(&config, request).await?, output, true)
        }
        TopCommand::Syslog(arguments) => {
            let request = stream_request(
                TaskKind::Syslog,
                cli.udid.clone(),
                arguments.output,
                if arguments.parse {
                    StreamFormat::JsonLines
                } else {
                    StreamFormat::Raw
                },
                None,
                None,
                None,
            )?;
            print_payload(request_auto(&config, request).await?, output, true)
        }
        TopCommand::Ostrace(arguments) => {
            let os_trace = os_trace_options(&arguments)?;
            let request = stream_request(
                TaskKind::OsTrace,
                cli.udid.clone(),
                arguments.output,
                if output.no_json {
                    StreamFormat::Raw
                } else {
                    StreamFormat::JsonLines
                },
                arguments.pid,
                arguments.process,
                Some(os_trace),
            )?;
            print_payload(request_auto(&config, request).await?, output, true)
        }
        TopCommand::Pcap(arguments) => {
            let request = stream_request(
                TaskKind::Pcap,
                cli.udid.clone(),
                arguments.output,
                StreamFormat::Pcap,
                arguments.pid,
                arguments.process,
                None,
            )?;
            print_payload(request_auto(&config, request).await?, output, true)
        }
        TopCommand::Tunnel { command } => {
            let request = tunnel_request(command, cli.udid.clone())?;
            print_payload(request_auto(&config, request).await?, output, true)
        }
        TopCommand::External(arguments) => {
            let argv = arguments
                .into_iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if argv.first().map(String::as_str) == Some("webinspector")
                && argv.get(1).map(String::as_str) == Some("js-shell")
            {
                return run_webinspector_js_shell(&config, &argv, cli.udid.clone(), output).await;
            }
            if argv.first().map(String::as_str) == Some("debug")
                && argv.get(1).map(String::as_str) != Some("proxy")
            {
                return run_debug_app(&config, &argv, cli.udid.clone(), output).await;
            }
            if let Some(request) = external_stream_request(&argv, cli.udid.clone())? {
                return print_payload(request_auto(&config, request).await?, output, true);
            }
            let invocation = CommandInvocation {
                argv,
                cwd: std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned()),
                udid: cli.udid.clone(),
                output: output.clone(),
            };
            let payload = request_auto(&config, Request::Execute { invocation }).await?;
            print_payload(payload, output, true)
        }
    }
}

fn server_config(cli: &Cli) -> anyhow::Result<ServerConfig> {
    let mut config = match &cli.server_home {
        Some(root) if root.is_absolute() => {
            ServerConfig::discover_with_paths(ServerPaths::new(root))?
        }
        Some(root) => bail!("--server-home must be absolute, got {}", root.display()),
        None => ServerConfig::discover()?,
    };
    if let Some(addr) = cli.server_addr {
        config.listen_addr = addr;
    }
    Ok(config)
}

async fn request_auto(
    config: &ServerConfig,
    request: Request,
) -> Result<ResponsePayload, ClientError> {
    let mut ensured = ensure_server(config).await?;
    if let Some(capability) = required_server_capability(&request)
        && !ensured
            .client
            .hello
            .capabilities
            .iter()
            .any(|value| value == capability)
    {
        return Err(ClientError::Protocol(ProtocolError::new(
            ErrorCode::NotImplemented,
            format!(
                "running server does not advertise {capability}; restart it with this ios binary"
            ),
        )));
    }
    ensured.client.request(request).await
}

fn required_server_capability(request: &Request) -> Option<&'static str> {
    match request {
        Request::WdaStart { .. } => Some("wda_event_capture"),
        Request::XCTestStart { spec }
            if spec.targets.iter().any(|target| {
                !target.is_ui_test
                    || !target.tests_to_run.is_empty()
                    || !target.tests_to_skip.is_empty()
            }) =>
        {
            Some("xctest_unit_filters")
        }
        Request::UiStreamStart { .. } => Some("persistent_ui_stream"),
        Request::UiRunnerStart { .. } => Some("persistent_ui_runner"),
        Request::AccessibilityInspectorStart { .. } => Some("persistent_accessibility_inspector"),
        Request::WebInspectorJsShellStart { .. } | Request::WebInspectorJsShellEval { .. } => {
            Some("interactive_webinspector_js_shell")
        }
        Request::TunnelStart { spec } if spec.mode == TunnelMode::Kernel => {
            Some("windows_kernel_tunnel")
        }
        Request::DproxyStart { .. } => Some("persistent_dproxy"),
        _ => None,
    }
}

async fn run_debug_app(
    config: &ServerConfig,
    arguments: &[String],
    udid: Option<String>,
    output: &OutputOptions,
) -> anyhow::Result<()> {
    let stop_at_entry = debug_app_stop_at_entry(arguments)?;
    let invocation = CommandInvocation {
        argv: arguments.to_vec(),
        cwd: std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        udid: udid.clone(),
        output: output.clone(),
    };
    let mut ensured = ensure_server(config).await?;
    if !ensured
        .client
        .hello
        .capabilities
        .iter()
        .any(|value| value == "debug_app_session")
    {
        return Err(ClientError::Protocol(ProtocolError::new(
            ErrorCode::NotImplemented,
            "running server does not advertise debug_app_session; restart it with this ios binary",
        ))
        .into());
    }
    let ResponsePayload::Command(preflight) = ensured
        .client
        .request(Request::Execute { invocation })
        .await?
    else {
        bail!("server returned an unexpected debug preflight response")
    };
    let app: DebugAppInfo = serde_json::from_value(
        preflight
            .value
            .ok_or_else(|| anyhow!("debug preflight returned no app metadata"))?,
    )
    .context("server returned invalid debug app metadata")?;
    let ResponsePayload::Task { task } = ensured
        .client
        .request(Request::DebugProxyStart {
            spec: DebugProxySpec {
                udid,
                bind_host: "127.0.0.1".into(),
                port: 0,
                client_owned: true,
            },
        })
        .await?
    else {
        bail!("server returned an unexpected debug proxy response")
    };
    let task_id = task.id;
    let session_result = async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut current = task;
        loop {
            match current.runtime_state {
                RuntimeState::Running => break,
                RuntimeState::Failed | RuntimeState::Stopped | RuntimeState::Completed => {
                    return Err(ClientError::Protocol(ProtocolError::new(
                        ErrorCode::TaskFailed,
                        current
                            .message
                            .unwrap_or_else(|| "debug proxy failed to start".into()),
                    ))
                    .into());
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(ClientError::Protocol(ProtocolError::new(
                        ErrorCode::Timeout,
                        format!(
                            "debug proxy task {task_id} did not become ready within 15 seconds: {}",
                            current.message.as_deref().unwrap_or("still starting")
                        ),
                    ))
                    .into());
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let ResponsePayload::Task { task } = ensured
                        .client
                        .request(Request::TaskShow { task_id })
                        .await?
                    else {
                        bail!("server returned an unexpected debug proxy task status")
                    };
                    current = task;
                }
            }
        }
        let host = current.runtime["host"]
            .as_str()
            .ok_or_else(|| anyhow!("debug proxy runtime has no host"))?;
        let port = current.runtime["port"]
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value != 0)
            .ok_or_else(|| anyhow!("debug proxy runtime has no valid port"))?;
        let directory = tempfile::Builder::new()
            .prefix("rust-ios-lldb-")
            .tempdir()
            .context("create temporary LLDB directory")?;
        let source_path = directory.path().join("debug.lldb");
        std::fs::write(&source_path, lldb_source(&app, host, port, stop_at_entry))
            .context("write LLDB source")?;
        if output.verbose {
            eprintln!(
                "debugging {} through {}:{} with {}",
                app.bundle_id,
                host,
                port,
                lldb_program().to_string_lossy()
            );
        }
        run_lldb(&source_path).await
    }
    .await;
    let stop_result = ensured
        .client
        .request(Request::TaskStop {
            selector: TaskSelector::Id(task_id),
        })
        .await;
    match (session_result, stop_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(error)) => Err(error.into()),
    }
}

fn debug_app_stop_at_entry(arguments: &[String]) -> anyhow::Result<bool> {
    let mut app_path = None;
    let mut stop_at_entry = false;
    for argument in arguments.iter().skip(1) {
        match argument.as_str() {
            "--stop-at-entry" if !stop_at_entry => stop_at_entry = true,
            "--stop-at-entry" => bail!("debug --stop-at-entry was provided more than once"),
            value if value.starts_with('-') => bail!("debug does not accept {value:?}"),
            value => {
                if app_path.replace(value).is_some() {
                    bail!("debug accepts exactly one app_path");
                }
            }
        }
    }
    if app_path.is_none() {
        bail!("debug requires app_path");
    }
    Ok(stop_at_entry)
}

fn lldb_source(app: &DebugAppInfo, host: &str, port: u16, stop_at_entry: bool) -> String {
    let local_executable = lldb_command_path(&app.local_executable_path);
    let device_executable = serde_json::to_string(&app.device_executable_path)
        .expect("serializing a string cannot fail");
    let stop_option = if stop_at_entry {
        " --stop-at-entry"
    } else {
        ""
    };
    format!(
        "platform select remote-ios\n\
target create -- \"{local_executable}\"\n\
script import lldb\n\
script lldb.debugger.GetSelectedTarget().GetModuleAtIndex(0).SetPlatformFileSpec(lldb.SBFileSpec({device_executable}))\n\
settings set target.env-vars OS_ACTIVITY_DT_MODE=enable\n\
process connect connect://{host}:{port}\n\
process launch{stop_option}\n"
    )
}

fn lldb_command_path(value: &str) -> String {
    value.replace('\\', "/").replace('"', "\\\"")
}

fn lldb_program() -> OsString {
    std::env::var_os("RUST_IOS_LLDB")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(if cfg!(windows) { "lldb.exe" } else { "lldb" }))
}

async fn run_lldb(source_path: &std::path::Path) -> anyhow::Result<()> {
    let program = lldb_program();
    let mut command = tokio::process::Command::new(&program);
    command
        .arg("-s")
        .arg(source_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        format!(
            "start LLDB {}; set RUST_IOS_LLDB to an LLDB executable when it is not on PATH",
            PathBuf::from(&program).display()
        )
    })?;
    let status = tokio::select! {
        status = child.wait() => status.context("wait for LLDB")?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to install Ctrl+C handler")?;
            let _ = child.kill().await;
            return Err(ClientError::Protocol(ProtocolError::new(
                ErrorCode::TaskFailed,
                "debug session interrupted",
            )).into());
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(ClientError::Protocol(ProtocolError::new(
            ErrorCode::TaskFailed,
            format!("LLDB exited with {status}"),
        ))
        .into())
    }
}

async fn run_webinspector_js_shell(
    config: &ServerConfig,
    arguments: &[String],
    udid: Option<String>,
    output: &OutputOptions,
) -> anyhow::Result<()> {
    let spec = webinspector_js_shell_spec(arguments, udid)?;
    let ready_timeout =
        std::time::Duration::from_millis(spec.command_timeout_ms.saturating_add(2_000));
    let mut ensured = ensure_server(config).await?;
    if !ensured
        .client
        .hello
        .capabilities
        .iter()
        .any(|value| value == "interactive_webinspector_js_shell")
    {
        return Err(ClientError::Protocol(ProtocolError::new(
            ErrorCode::NotImplemented,
            "running server does not advertise interactive_webinspector_js_shell; restart it with this ios binary",
        ))
        .into());
    }
    let ResponsePayload::Task { task } = ensured
        .client
        .request(Request::WebInspectorJsShellStart { spec })
        .await?
    else {
        bail!("server returned an unexpected response when starting WebInspector JS shell")
    };
    let task_id = task.id;
    let session_result = async {
        let deadline = tokio::time::Instant::now() + ready_timeout;
        let mut current = task;
        loop {
            match current.runtime_state {
                rust_ios_protocol::RuntimeState::Running => break,
                rust_ios_protocol::RuntimeState::Failed
                | rust_ios_protocol::RuntimeState::Stopped
                | rust_ios_protocol::RuntimeState::Completed => {
                    return Err(ClientError::Protocol(ProtocolError::new(
                        ErrorCode::TaskFailed,
                        current
                            .message
                            .unwrap_or_else(|| "WebInspector JS shell failed to start".into()),
                    ))
                    .into());
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(ClientError::Protocol(ProtocolError::new(
                        ErrorCode::Timeout,
                        format!(
                            "WebInspector JS shell task {task_id} did not become ready within {:.3} seconds: {}",
                            ready_timeout.as_secs_f64(),
                            current.message.as_deref().unwrap_or("still starting")
                        ),
                    ))
                    .into());
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let ResponsePayload::Task { task } = ensured
                        .client
                        .request(Request::TaskShow { task_id })
                        .await?
                    else {
                        bail!("server returned an unexpected JS shell task status")
                    };
                    current = task;
                }
            }
        }

        let interactive = std::io::stdin().is_terminal();
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        loop {
            if interactive {
                eprint!("> ");
                std::io::stderr().flush()?;
            }
            let line = tokio::select! {
                signal = &mut interrupt => {
                    signal.context("failed to install Ctrl+C handler")?;
                    break;
                }
                line = lines.next_line() => {
                    line.context("failed reading WebInspector JS shell input")?
                }
            };
            let Some(line) = line else {
                break;
            };
            let expression = line.trim();
            if expression.is_empty() {
                continue;
            }
            if matches!(expression, ".exit" | "exit" | "quit") {
                break;
            }
            match ensured
                .client
                .request(Request::WebInspectorJsShellEval {
                    task_id,
                    expression: expression.to_owned(),
                })
                .await
            {
                Ok(payload) => print_payload(payload, output, true)?,
                Err(error @ ClientError::Protocol(_)) => {
                    print_error(&anyhow::Error::new(error), output);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    .await;
    let stop_result = ensured
        .client
        .request(Request::TaskStop {
            selector: TaskSelector::Id(task_id),
        })
        .await;
    match (session_result, stop_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Ok(_)) => Ok(()),
        (Ok(()), Err(error)) => Err(error.into()),
    }
}

fn webinspector_js_shell_spec(
    arguments: &[String],
    udid: Option<String>,
) -> anyhow::Result<WebInspectorJsShellSpec> {
    let mut url = None;
    let mut bundle_id = None;
    let mut open_safari = false;
    let mut timeout_seconds = 5.0_f64;
    let mut timeout_seen = false;
    let mut index = 2;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--bundle-id" | "--timeout" => {
                let option = arguments[index].clone();
                let value = arguments
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("webinspector js-shell {option} requires a value"))?;
                if option == "--bundle-id" {
                    if bundle_id.replace(value.clone()).is_some() {
                        bail!("webinspector js-shell --bundle-id was provided more than once");
                    }
                } else {
                    if timeout_seen {
                        bail!("webinspector js-shell --timeout was provided more than once");
                    }
                    timeout_seconds = value.parse::<f64>().map_err(|_| {
                        anyhow!("webinspector js-shell --timeout must be a positive number")
                    })?;
                    timeout_seen = true;
                }
                index += 2;
            }
            "--open-safari" => {
                if open_safari {
                    bail!("webinspector js-shell --open-safari was provided more than once");
                }
                open_safari = true;
                index += 1;
            }
            value if value.starts_with("--bundle-id=") => {
                let value = value.trim_start_matches("--bundle-id=");
                if value.is_empty() || bundle_id.replace(value.to_owned()).is_some() {
                    bail!("webinspector js-shell --bundle-id requires one non-empty value");
                }
                index += 1;
            }
            value if value.starts_with("--timeout=") => {
                if timeout_seen {
                    bail!("webinspector js-shell --timeout was provided more than once");
                }
                timeout_seconds = value
                    .trim_start_matches("--timeout=")
                    .parse::<f64>()
                    .map_err(|_| {
                        anyhow!("webinspector js-shell --timeout must be a positive number")
                    })?;
                timeout_seen = true;
                index += 1;
            }
            value if value.starts_with('-') => {
                bail!("webinspector js-shell does not accept {value:?}");
            }
            value => {
                if url.replace(value.to_owned()).is_some() {
                    bail!("webinspector js-shell accepts at most one URL");
                }
                index += 1;
            }
        }
    }
    if !timeout_seconds.is_finite() || !(1.0..=120.0).contains(&timeout_seconds) {
        bail!("webinspector js-shell --timeout must be between 1 and 120 seconds");
    }
    Ok(WebInspectorJsShellSpec {
        udid,
        url,
        bundle_id,
        open_safari,
        command_timeout_ms: (timeout_seconds * 1_000.0).round() as u64,
        output_path: None,
    })
}

async fn listen_devices(config: &ServerConfig, output: &OutputOptions) -> anyhow::Result<()> {
    let mut ensured = ensure_server(config).await?;
    let initial = ensured.client.request(Request::ListenSnapshot).await?;
    let ResponsePayload::Devices(mut previous) = initial else {
        bail!("server returned an unexpected response to listen snapshot")
    };
    print_payload(ResponsePayload::Devices(previous.clone()), output, false)?;
    loop {
        let response = tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to install Ctrl+C handler")?;
                return Ok(());
            }
            response = ensured.client.request(Request::WaitDeviceChange {
                previous: previous.clone(),
                timeout_ms: 2_000,
            }) => response?,
        };
        let ResponsePayload::Devices(next) = response else {
            bail!("server returned an unexpected response to device change wait")
        };
        if next != previous {
            print_payload(ResponsePayload::Devices(next.clone()), output, false)?;
            previous = next;
        }
    }
}

fn forward_request(arguments: ForwardArgs, udid: Option<String>) -> anyhow::Result<Request> {
    if arguments
        .values
        .first()
        .is_some_and(|value| value == "list")
    {
        return Ok(Request::TaskList {
            kind: Some(TaskKind::Forward),
        });
    }
    if arguments
        .values
        .first()
        .is_some_and(|value| value == "clear")
    {
        return Ok(Request::TaskStop {
            selector: TaskSelector::Kind(TaskKind::Forward),
        });
    }
    if arguments
        .values
        .first()
        .is_some_and(|value| value == "stop")
    {
        let selector = arguments
            .values
            .get(1)
            .ok_or_else(|| anyhow!("forward stop requires task id or host port"))?;
        let selector = if let Ok(id) = selector.parse::<Uuid>() {
            TaskSelector::Id(id)
        } else {
            TaskSelector::ForwardPort(selector.parse().context("invalid host port")?)
        };
        return Ok(Request::TaskStop { selector });
    }

    let mut pairs = Vec::new();
    if arguments.values.len() == 2 {
        pairs.push((
            arguments.values[0].parse().context("invalid host port")?,
            arguments.values[1].parse().context("invalid target port")?,
        ));
    } else if !arguments.values.is_empty() {
        bail!("forward expects <hostPort> <targetPort>, --port mappings, list, stop or clear");
    }
    for mapping in arguments.mappings {
        pairs.push(parse_port_mapping(&mapping)?);
    }
    if pairs.is_empty() {
        bail!("forward requires at least one port mapping");
    }
    let connection = match (arguments.usb, arguments.network) {
        (true, false) => ConnectionPreference::UsbOnly,
        (false, true) => ConnectionPreference::NetworkOnly,
        (true, true) => ConnectionPreference::Any,
        (false, false) => ConnectionPreference::UsbPreferred,
    };
    Ok(Request::ForwardStart {
        specs: pairs
            .into_iter()
            .map(|(host_port, device_port)| ForwardSpec {
                udid: udid.clone(),
                bind_host: arguments.bind.clone(),
                host_port,
                device_port,
                connection: connection.clone(),
            })
            .collect(),
    })
}

fn stream_request(
    kind: TaskKind,
    udid: Option<String>,
    output: Option<PathBuf>,
    format: StreamFormat,
    pid: Option<u32>,
    process: Option<String>,
    os_trace: Option<OsTraceOptions>,
) -> anyhow::Result<Request> {
    let output_path = match output {
        Some(path) => {
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            Some(path.to_string_lossy().into_owned())
        }
        None => None,
    };
    Ok(Request::StreamStart {
        spec: StreamTaskSpec {
            kind,
            udid,
            output_path,
            format,
            pid,
            process,
            os_trace,
        },
    })
}

fn nonempty_filter(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn os_trace_options(arguments: &OsTraceArgs) -> anyhow::Result<OsTraceOptions> {
    Ok(OsTraceOptions {
        levels: parse_os_trace_levels(arguments.level.as_deref())?,
        subsystem_contains: nonempty_filter(arguments.subsystem.clone()),
        message_contains: nonempty_filter(arguments.match_filter.clone()),
        message_excludes: nonempty_filter(arguments.exclude.clone()),
        follow: arguments.follow,
    })
}

fn parse_os_trace_levels(value: Option<&str>) -> anyhow::Result<Vec<OsTraceLevel>> {
    let mut levels = Vec::new();
    for raw in value.unwrap_or_default().split(',') {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let level = match name.as_str() {
            "default" => OsTraceLevel::Default,
            "info" => OsTraceLevel::Info,
            "debug" => OsTraceLevel::Debug,
            "error" => OsTraceLevel::Error,
            "fault" => OsTraceLevel::Fault,
            _ => bail!(
                "unknown ostrace level {name:?}; valid levels: default, info, debug, error, fault"
            ),
        };
        if !levels.contains(&level) {
            levels.push(level);
        }
    }
    Ok(levels)
}

fn external_stream_request(
    arguments: &[String],
    udid: Option<String>,
) -> anyhow::Result<Option<Request>> {
    if arguments.first().map(String::as_str) == Some("dproxy") {
        let value_options = [
            "--mode",
            "--iface",
            "--bind",
            "--port",
            "--upstream",
            "--output",
        ];
        validate_external_options(arguments, &value_options, &["--binary"])?;
        for option in value_options {
            if external_option_values(arguments, option).len() > 1 {
                bail!("dproxy {option} was provided more than once");
            }
        }
        if arguments
            .iter()
            .filter(|value| *value == "--binary")
            .count()
            > 1
        {
            bail!("dproxy --binary was provided more than once");
        }
        let mode = match external_option_value(arguments, "--mode")
            .unwrap_or_else(|| "all".into())
            .as_str()
        {
            "all" => DproxyMode::All,
            "usbmuxd" => DproxyMode::Usbmuxd,
            "utun" => DproxyMode::Utun,
            value => bail!("dproxy --mode must be all, usbmuxd or utun, got {value:?}"),
        };
        let port = external_option_value(arguments, "--port")
            .unwrap_or_else(|| "0".into())
            .parse::<u16>()
            .map_err(|_| anyhow!("dproxy --port must be 0..65535"))?;
        let output_path = external_option_value(arguments, "--output")
            .map(PathBuf::from)
            .map(resolve_client_path)
            .transpose()?
            .map(|path| path.to_string_lossy().into_owned());
        return Ok(Some(Request::DproxyStart {
            spec: DproxySpec {
                bind_host: external_option_value(arguments, "--bind")
                    .unwrap_or_else(|| "127.0.0.1".into()),
                port,
                upstream_address: external_option_value(arguments, "--upstream")
                    .unwrap_or_else(|| "127.0.0.1:27015".into()),
                binary_only: arguments.iter().any(|value| value == "--binary"),
                mode,
                interface: external_option_value(arguments, "--iface"),
                output_path,
            },
        }));
    }
    if arguments.first().map(String::as_str) == Some("setlocationgpx") {
        return location_simulation_request(arguments, udid).map(Some);
    }
    if arguments.first().map(String::as_str) == Some("ax")
        && arguments.get(1).map(String::as_str) != Some("audit")
    {
        validate_external_options(arguments, &["--font"], &[])?;
        if external_option_values(arguments, "--font").len() > 1 {
            bail!("ax --font was provided more than once");
        }
        let font_size = external_option_value(arguments, "--font")
            .map(|value| {
                value
                    .parse::<f64>()
                    .map_err(|_| anyhow!("ax --font must be a number, got {value:?}"))
            })
            .transpose()?;
        if font_size.is_some_and(|value| !value.is_finite() || value <= 0.0) {
            bail!("ax --font must be a finite number greater than zero");
        }
        return Ok(Some(Request::AccessibilityInspectorStart {
            spec: AccessibilityInspectorSpec {
                udid,
                output_path: None,
                font_size,
            },
        }));
    }
    if arguments.first().map(String::as_str) == Some("ui")
        && arguments.get(1).map(String::as_str) == Some("run")
    {
        return ui_runner_request(arguments, udid).map(Some);
    }
    if arguments.first().map(String::as_str) == Some("ui")
        && arguments.get(1).map(String::as_str) == Some("stream")
    {
        return ui_stream_request(arguments).map(Some);
    }
    if arguments.first().map(String::as_str) == Some("webinspector")
        && arguments.get(1).map(String::as_str) == Some("cdp")
    {
        return webinspector_cdp_request(arguments, udid).map(Some);
    }
    if arguments.first().map(String::as_str) == Some("launch") && launch_wait_requested(arguments) {
        return app_launch_request(arguments, udid).map(Some);
    }
    if matches!(
        arguments.first().map(String::as_str),
        Some("runtest" | "runxctest")
    ) {
        return xctest_request(arguments, udid).map(Some);
    }
    if arguments.first().map(String::as_str) == Some("runwda") {
        let mut index = 1;
        while index < arguments.len() {
            match arguments[index].as_str() {
                "--bundleid"
                | "--testrunnerbundleid"
                | "--xctestconfig"
                | "--arg"
                | "--env"
                | "--log-output" => {
                    if arguments.get(index + 1).is_none() {
                        bail!("{} requires a value", arguments[index]);
                    }
                    index += 2;
                }
                value
                    if value.starts_with("--bundleid=")
                        || value.starts_with("--testrunnerbundleid=")
                        || value.starts_with("--xctestconfig=")
                        || value.starts_with("--arg=")
                        || value.starts_with("--env=")
                        || value.starts_with("--log-output=") =>
                {
                    index += 1;
                }
                value => bail!("runwda does not accept {value:?}"),
            }
        }

        let target_bundle_id = external_option_value(arguments, "--bundleid");
        let requested_runner = external_option_value(arguments, "--testrunnerbundleid");
        let xctest_config = external_option_value(arguments, "--xctestconfig");
        let (runner_bundle_id, target_bundle_id, xctest_config) =
            match (target_bundle_id, requested_runner, xctest_config) {
                (None, None, None) => (
                    "com.facebook.WebDriverAgentRunner.xctrunner".into(),
                    Some("com.facebook.WebDriverAgentRunner.xctrunner".into()),
                    Some("WebDriverAgentRunner.xctest".into()),
                ),
                (target, runner, config) => {
                    let runner = runner.or_else(|| target.clone()).ok_or_else(|| {
                        anyhow!("runwda --xctestconfig requires --testrunnerbundleid or --bundleid")
                    })?;
                    (runner.clone(), Some(target.unwrap_or(runner)), config)
                }
            };
        let mut environment = BTreeMap::new();
        for value in external_option_values(arguments, "--env") {
            let (key, value) = value
                .split_once('=')
                .ok_or_else(|| anyhow!("runwda --env must be KEY=VALUE, got {value:?}"))?;
            if key.is_empty() {
                bail!("runwda --env key cannot be empty");
            }
            environment.insert(key.to_owned(), value.to_owned());
        }
        return Ok(Some(Request::WdaStart {
            spec: WdaSpec {
                udid,
                runner_bundle_id,
                target_bundle_id,
                xctest_config,
                arguments: external_option_values(arguments, "--arg"),
                environment,
                output_path: xctest_output_path(arguments)?,
                readiness_timeout_ms: 30_000,
            },
        }));
    }
    if arguments.first().map(String::as_str) == Some("debug")
        && arguments.get(1).map(String::as_str) == Some("proxy")
    {
        let mut index = 2;
        while index < arguments.len() {
            match arguments[index].as_str() {
                "--bind" | "--port" => {
                    if arguments
                        .get(index + 1)
                        .is_none_or(|value| value.starts_with('-'))
                    {
                        bail!("{} requires a value", arguments[index]);
                    }
                    index += 2;
                }
                value if value.starts_with("--bind=") || value.starts_with("--port=") => {
                    index += 1;
                }
                value => bail!("debug proxy does not accept {value:?}"),
            }
        }
        let port = external_option_value(arguments, "--port")
            .unwrap_or_else(|| "0".into())
            .parse::<u16>()
            .map_err(|_| anyhow!("debug proxy --port must be 0..65535"))?;
        let bind_host =
            external_option_value(arguments, "--bind").unwrap_or_else(|| "127.0.0.1".into());
        return Ok(Some(Request::DebugProxyStart {
            spec: DebugProxySpec {
                udid,
                bind_host,
                port,
                client_owned: false,
            },
        }));
    }
    if arguments.first().map(String::as_str) == Some("screenshot")
        && arguments.iter().any(|value| value == "--stream")
    {
        if arguments
            .iter()
            .any(|value| value == "--output" || value.starts_with("--output="))
        {
            bail!("screenshot --stream does not accept --output");
        }
        let port = external_option_value(arguments, "--port")
            .unwrap_or_else(|| "3333".into())
            .parse::<u16>()
            .map_err(|_| anyhow!("screenshot --port must be 0..65535"))?;
        let bind_host =
            external_option_value(arguments, "--bind").unwrap_or_else(|| "127.0.0.1".into());
        return Ok(Some(Request::ScreenshotStreamStart {
            spec: ScreenshotStreamSpec {
                udid,
                bind_host,
                port,
                interval_ms: 100,
            },
        }));
    }
    let kind = match arguments {
        [command, action] if command == "instruments" && action == "notifications" => {
            TaskKind::Notifications
        }
        [command] if command == "sysmontap" => TaskKind::Sysmontap,
        _ => return Ok(None),
    };
    stream_request(kind, udid, None, StreamFormat::JsonLines, None, None, None).map(Some)
}

fn location_simulation_request(
    arguments: &[String],
    udid: Option<String>,
) -> anyhow::Result<Request> {
    validate_external_options(arguments, &["--gpxfilepath"], &[])?;
    if external_option_values(arguments, "--gpxfilepath").len() != 1 {
        bail!("setlocationgpx requires exactly one --gpxfilepath");
    }
    let raw_path = external_option_value(arguments, "--gpxfilepath")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("setlocationgpx --gpxfilepath cannot be empty"))?;
    let path = resolve_client_path(PathBuf::from(raw_path))?;
    let points = gpx::parse(&path)?;
    Ok(Request::LocationSimulationStart {
        spec: LocationSimulationSpec {
            udid,
            source_path: path.to_string_lossy().into_owned(),
            points,
            output_path: None,
        },
    })
}

fn ui_runner_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    let target = match arguments.get(2).map(String::as_str) {
        Some("wda") => UiRunnerTarget::Wda,
        Some("devicekit") => UiRunnerTarget::DeviceKit,
        Some(value) => bail!("ui run target must be wda or devicekit, got {value:?}"),
        None => bail!("ui run requires wda or devicekit"),
    };
    validate_external_options(
        &arguments[2..],
        &[
            "--bundleid",
            "--test-runner-bundleid",
            "--xctest-config",
            "--host-port",
            "--log-output",
            "--bind",
        ],
        &[],
    )?;
    for option in [
        "--bundleid",
        "--test-runner-bundleid",
        "--xctest-config",
        "--host-port",
        "--log-output",
        "--bind",
    ] {
        if external_option_values(arguments, option).len() > 1 {
            bail!("ui run option {option} was provided more than once");
        }
    }
    let (default_bundle, default_port, health_path, default_config) = match target {
        UiRunnerTarget::Wda => (
            "com.deviceboxhq.goios.WebDriverAgentRunner.xctrunner",
            8100,
            "/status",
            "WebDriverAgentRunner.xctest",
        ),
        UiRunnerTarget::DeviceKit => (
            "com.deviceboxhq.goios.devicekit.runner",
            12004,
            "/health",
            "devicekit-iosUITests.xctest",
        ),
    };
    let target_bundle_id = external_option_value(arguments, "--bundleid")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_bundle.into());
    let runner_bundle_id = external_option_value(arguments, "--test-runner-bundleid")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| target_bundle_id.clone());
    let xctest_config = external_option_value(arguments, "--xctest-config")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_config.into());
    let host_port = external_option_value(arguments, "--host-port")
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("ui run --host-port must be 0..65535"))
        })
        .transpose()?
        .unwrap_or(default_port);
    let bind_host = external_option_value(arguments, "--bind")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".into());
    let output_path = match external_option_value(arguments, "--log-output") {
        Some(path) if path != "-" => Some(
            resolve_client_path(PathBuf::from(path))?
                .to_string_lossy()
                .into_owned(),
        ),
        _ => None,
    };
    Ok(Request::UiRunnerStart {
        spec: UiRunnerSpec {
            target,
            udid,
            runner_bundle_id,
            target_bundle_id,
            xctest_config,
            bind_host,
            host_port,
            device_port: default_port,
            health_path: health_path.into(),
            readiness_timeout_ms: 120_000,
            output_path,
        },
    })
}

fn ui_stream_request(arguments: &[String]) -> anyhow::Result<Request> {
    let format = match arguments.get(2).map(String::as_str) {
        Some("mjpeg") => UiStreamFormat::Mjpeg,
        Some("h264") => UiStreamFormat::H264,
        Some(value) if !value.starts_with('-') => {
            bail!("ui stream format must be mjpeg or h264, got {value:?}")
        }
        _ => bail!("ui stream requires mjpeg or h264"),
    };
    validate_external_options(
        &arguments[2..],
        &[
            "--fps",
            "--quality",
            "--scale",
            "--bitrate",
            "--driver",
            "--wda-url",
            "--devicekit-url",
            "--output",
        ],
        &[],
    )?;
    for option in [
        "--fps",
        "--quality",
        "--scale",
        "--bitrate",
        "--driver",
        "--wda-url",
        "--devicekit-url",
        "--output",
    ] {
        if external_option_values(arguments, option).len() > 1 {
            bail!("ui stream option {option} was provided more than once");
        }
    }

    let driver_name = external_option_value(arguments, "--driver")
        .or_else(|| std::env::var("GO_IOS_UI_DRIVER").ok())
        .unwrap_or_else(|| "devicekit".into())
        .to_ascii_lowercase();
    let driver = match driver_name.as_str() {
        "wda" => UiAutomationDriver::Wda,
        "devicekit" => UiAutomationDriver::DeviceKit,
        "auto" => UiAutomationDriver::Auto,
        _ => bail!("ui stream --driver must be wda, devicekit or auto"),
    };
    if driver == UiAutomationDriver::Wda && format == UiStreamFormat::H264 {
        bail!("WDA ui stream supports mjpeg only; use --driver=devicekit for h264");
    }

    let wda_url = nonempty_ui_stream_option(
        arguments,
        "--wda-url",
        "GO_IOS_WDA_URL",
        "http://127.0.0.1:8100",
    )?;
    let devicekit_url = nonempty_ui_stream_option(
        arguments,
        "--devicekit-url",
        "GO_IOS_DEVICEKIT_URL",
        "http://127.0.0.1:12004",
    )?;
    let output_path = match external_option_value(arguments, "--output") {
        Some(value) if value == "-" => None,
        Some(value) if value.trim().is_empty() => bail!("ui stream --output cannot be empty"),
        Some(value) => Some(
            resolve_client_path(PathBuf::from(value))?
                .to_string_lossy()
                .into_owned(),
        ),
        None => None,
    };

    Ok(Request::UiStreamStart {
        spec: UiStreamSpec {
            driver,
            format,
            wda_url,
            devicekit_url,
            wda_task_id: None,
            output_path,
            fps: optional_bounded_u32(arguments, "--fps", 1, 240)?,
            quality: optional_bounded_u32(arguments, "--quality", 1, 100)?,
            scale: optional_positive_float(arguments, "--scale")?,
            bitrate: optional_bounded_u32(arguments, "--bitrate", 1, u32::MAX)?,
        },
    })
}

fn nonempty_ui_stream_option(
    arguments: &[String],
    option: &str,
    environment: &str,
    default: &str,
) -> anyhow::Result<String> {
    let value = external_option_value(arguments, option)
        .or_else(|| std::env::var(environment).ok())
        .unwrap_or_else(|| default.into());
    if value.trim().is_empty() {
        bail!("ui stream {option} cannot be empty");
    }
    Ok(value)
}

fn optional_bounded_u32(
    arguments: &[String],
    option: &str,
    minimum: u32,
    maximum: u32,
) -> anyhow::Result<Option<u32>> {
    let Some(value) = external_option_value(arguments, option) else {
        return Ok(None);
    };
    let parsed = value
        .parse::<u32>()
        .map_err(|_| anyhow!("ui stream {option} must be an integer"))?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("ui stream {option} must be between {minimum} and {maximum}");
    }
    Ok(Some(parsed))
}

fn optional_positive_float(arguments: &[String], option: &str) -> anyhow::Result<Option<f64>> {
    let Some(value) = external_option_value(arguments, option) else {
        return Ok(None);
    };
    let parsed = value
        .parse::<f64>()
        .map_err(|_| anyhow!("ui stream {option} must be a number"))?;
    if !parsed.is_finite() || parsed <= 0.0 {
        bail!("ui stream {option} must be a finite positive number");
    }
    Ok(Some(parsed))
}

fn webinspector_cdp_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    let mut index = 2;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--host" | "--port" | "--timeout" => {
                if arguments
                    .get(index + 1)
                    .is_none_or(|value| value.starts_with('-'))
                {
                    bail!("{} requires a value", arguments[index]);
                }
                index += 2;
            }
            value
                if value.starts_with("--host=")
                    || value.starts_with("--port=")
                    || value.starts_with("--timeout=") =>
            {
                index += 1;
            }
            value => bail!("webinspector cdp does not accept {value:?}"),
        }
    }
    let bind_host = match external_option_value(arguments, "--host") {
        Some(value) if value.trim().is_empty() => {
            bail!("webinspector cdp --host cannot be empty")
        }
        Some(value) => value,
        None => "127.0.0.1".into(),
    };
    let port = external_option_value(arguments, "--port")
        .unwrap_or_else(|| "9222".into())
        .parse::<u16>()
        .map_err(|_| anyhow!("webinspector cdp --port must be 0..65535"))?;
    let timeout_seconds = external_option_value(arguments, "--timeout")
        .unwrap_or_else(|| "15".into())
        .parse::<f64>()
        .map_err(|_| anyhow!("webinspector cdp --timeout must be a positive number"))?;
    if !timeout_seconds.is_finite() || !(1.0..=120.0).contains(&timeout_seconds) {
        bail!("webinspector cdp --timeout must be between 1 and 120 seconds");
    }
    Ok(Request::WebInspectorCdpStart {
        spec: WebInspectorCdpSpec {
            udid,
            bind_host,
            port,
            startup_timeout_ms: (timeout_seconds * 1_000.0).round() as u64,
        },
    })
}

fn launch_wait_requested(arguments: &[String]) -> bool {
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--wait" => return true,
            "--arg" | "--env" => index += 2,
            _ => index += 1,
        }
    }
    false
}

fn app_launch_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    let mut bundle_id = None;
    let mut app_arguments = Vec::new();
    let mut environment = BTreeMap::new();
    let mut kill_existing = false;
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "--wait" => index += 1,
            "--kill-existing" => {
                kill_existing = true;
                index += 1;
            }
            "--arg" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("launch --arg requires a value"))?;
                app_arguments.push(value.clone());
                index += 2;
            }
            "--env" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("launch --env requires KEY=VALUE"))?;
                insert_launch_environment(&mut environment, value)?;
                index += 2;
            }
            value if value.starts_with("--arg=") => {
                app_arguments.push(value["--arg=".len()..].to_owned());
                index += 1;
            }
            value if value.starts_with("--env=") => {
                insert_launch_environment(&mut environment, &value["--env=".len()..])?;
                index += 1;
            }
            value if value.starts_with('-') => {
                bail!("launch --wait does not accept {value:?}");
            }
            value => {
                if bundle_id.replace(value.to_owned()).is_some() {
                    bail!("launch --wait accepts exactly one bundle ID");
                }
                index += 1;
            }
        }
    }
    let bundle_id = bundle_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("launch --wait requires a bundle ID"))?;
    Ok(Request::AppLaunchStart {
        spec: AppLaunchSpec {
            udid,
            bundle_id,
            arguments: app_arguments,
            environment,
            kill_existing,
            output_path: None,
        },
    })
}

fn insert_launch_environment(
    environment: &mut BTreeMap<String, String>,
    value: &str,
) -> anyhow::Result<()> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| anyhow!("launch --env must be KEY=VALUE, got {value:?}"))?;
    if key.is_empty() || key.contains('=') {
        bail!("launch --env key is invalid: {key:?}");
    }
    environment.insert(key.to_owned(), value.to_owned());
    Ok(())
}

fn external_option_value(arguments: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for (index, argument) in arguments.iter().enumerate() {
        if let Some(value) = argument.strip_prefix(&prefix) {
            return Some(value.to_owned());
        }
        if argument == name {
            return arguments.get(index + 1).cloned();
        }
    }
    None
}

fn external_option_values(arguments: &[String], name: &str) -> Vec<String> {
    let prefix = format!("{name}=");
    let mut values = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        if let Some(value) = arguments[index].strip_prefix(&prefix) {
            values.push(value.to_owned());
            index += 1;
        } else if arguments[index] == name {
            if let Some(value) = arguments.get(index + 1) {
                values.push(value.clone());
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    values
}

fn xctest_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    match arguments.first().map(String::as_str) {
        Some("runtest") => runtest_request(arguments, udid),
        Some("runxctest") => runxctest_request(arguments, udid),
        _ => bail!("XCTest command is missing"),
    }
}

fn runtest_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    validate_external_options(
        arguments,
        &[
            "--bundle-id",
            "--test-runner-bundle-id",
            "--xctest-config",
            "--log-output",
            "--test-to-run",
            "--test-to-skip",
            "--env",
        ],
        &["--xctest"],
    )?;
    let runner_bundle_id = external_option_value(arguments, "--test-runner-bundle-id")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("runtest requires --test-runner-bundle-id"))?;
    let xctest_config = external_option_value(arguments, "--xctest-config")
        .filter(|value| !value.trim().is_empty());
    let target_bundle_id =
        external_option_value(arguments, "--bundle-id").filter(|value| !value.trim().is_empty());
    let is_ui_test = !arguments.iter().any(|value| value == "--xctest");
    if !is_ui_test
        && xctest_config
            .as_deref()
            .and_then(|name| name.strip_suffix(".xctest"))
            .is_none_or(str::is_empty)
    {
        bail!("runtest --xctest requires --xctest-config with a non-empty .xctest bundle name");
    }
    if !is_ui_test && target_bundle_id.is_some() {
        bail!("runtest --xctest uses the runner as test host and does not accept --bundle-id");
    }
    let name = xctest_config
        .as_deref()
        .and_then(|value| value.strip_suffix(".xctest"))
        .filter(|value| !value.is_empty())
        .unwrap_or(&runner_bundle_id)
        .to_owned();
    let mut environment = BTreeMap::new();
    for value in external_option_values(arguments, "--env") {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("runtest --env must be KEY=VALUE, got {value:?}"))?;
        if key.is_empty() {
            bail!("runtest --env key cannot be empty");
        }
        environment.insert(
            key.to_owned(),
            XCTestEnvironmentValue::String(value.to_owned()),
        );
    }
    Ok(Request::XCTestStart {
        spec: XCTestSpec {
            udid,
            targets: vec![XCTestTargetSpec {
                name,
                runner_bundle_id,
                target_bundle_id,
                target_app_name: None,
                xctest_config,
                arguments: Vec::new(),
                environment,
                tests_to_run: external_option_values(arguments, "--test-to-run"),
                tests_to_skip: external_option_values(arguments, "--test-to-skip"),
                is_ui_test,
            }],
            output_path: xctest_output_path(arguments)?,
            source_xctestrun_path: None,
            timeout_ms: None,
        },
    })
}

fn runxctest_request(arguments: &[String], udid: Option<String>) -> anyhow::Result<Request> {
    validate_external_options(arguments, &["--xctestrun-file-path", "--log-output"], &[])?;
    let source = external_option_value(arguments, "--xctestrun-file-path")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("runxctest requires --xctestrun-file-path"))?;
    let source = resolve_client_path(PathBuf::from(source))?;
    let targets = xctestrun::parse_xctestrun(&source)?;
    Ok(Request::XCTestStart {
        spec: XCTestSpec {
            udid,
            targets,
            output_path: xctest_output_path(arguments)?,
            source_xctestrun_path: Some(source.to_string_lossy().into_owned()),
            timeout_ms: None,
        },
    })
}

fn xctest_output_path(arguments: &[String]) -> anyhow::Result<Option<String>> {
    let Some(path) = external_option_value(arguments, "--log-output") else {
        return Ok(None);
    };
    if path.is_empty() {
        bail!("--log-output cannot be empty");
    }
    if path == "-" {
        return Ok(None);
    }
    Ok(Some(
        resolve_client_path(PathBuf::from(path))?
            .to_string_lossy()
            .into_owned(),
    ))
}

fn resolve_client_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_external_options(
    arguments: &[String],
    value_options: &[&str],
    flags: &[&str],
) -> anyhow::Result<()> {
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if flags.contains(&argument) {
            index += 1;
            continue;
        }
        if value_options.contains(&argument) {
            if arguments.get(index + 1).is_none() {
                bail!("{argument} requires a value");
            }
            index += 2;
            continue;
        }
        if value_options
            .iter()
            .any(|option| argument.starts_with(&format!("{option}=")))
        {
            index += 1;
            continue;
        }
        bail!("{} does not accept {argument:?}", arguments[0]);
    }
    Ok(())
}

fn parse_port_mapping(value: &str) -> anyhow::Result<(u16, u16)> {
    let separator = if value.contains(':') { ':' } else { '=' };
    let (host, target) = value
        .split_once(separator)
        .ok_or_else(|| anyhow!("invalid port mapping {value:?}; expected host:target"))?;
    Ok((host.parse()?, target.parse()?))
}

fn tunnel_request(command: TunnelCommand, udid: Option<String>) -> anyhow::Result<Request> {
    Ok(match command {
        TunnelCommand::Start {
            userspace,
            remote_pairing,
            pair_record_path,
            userspace_bind,
            userspace_port,
            interface_name,
            wintun_path,
        } => {
            if (userspace || remote_pairing) && (interface_name.is_some() || wintun_path.is_some())
            {
                bail!("--interface-name and --wintun-path require kernel tunnel mode");
            }
            Request::TunnelStart {
                spec: TunnelSpec {
                    udid,
                    mode: if userspace || remote_pairing {
                        TunnelMode::Userspace
                    } else {
                        TunnelMode::Kernel
                    },
                    transport: if remote_pairing {
                        TunnelTransport::RemotePairingTcp
                    } else {
                        TunnelTransport::UsbCoreDevice
                    },
                    pair_record_path: resolve_pair_record_path(pair_record_path)?,
                    userspace_bind_host: userspace_bind,
                    userspace_port,
                    kernel_interface_name: interface_name,
                    wintun_path: wintun_path
                        .map(resolve_client_path)
                        .transpose()?
                        .map(|path| path.to_string_lossy().into_owned()),
                },
            }
        }
        TunnelCommand::Pair {
            manual,
            pair_record_path,
        } => Request::TunnelPair {
            spec: TunnelPairSpec {
                udid,
                pair_record_path: resolve_pair_record_path(pair_record_path)?,
                manual,
            },
        },
        TunnelCommand::List => Request::TaskList {
            kind: Some(TaskKind::Tunnel),
        },
        TunnelCommand::Stop | TunnelCommand::Stopagent => Request::TaskStop {
            selector: udid
                .map(TaskSelector::TunnelUdid)
                .unwrap_or(TaskSelector::Kind(TaskKind::Tunnel)),
        },
        TunnelCommand::Refresh => Request::TunnelRefresh { udid },
    })
}

fn resolve_pair_record_path(value: String) -> anyhow::Result<String> {
    if value.trim().is_empty() || matches!(value.trim(), "." | "default") {
        return Ok("default".into());
    }
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("cannot resolve relative pair record path")?
            .join(path)
    };
    Ok(path.to_string_lossy().into_owned())
}

fn parse_task_kind(value: String) -> anyhow::Result<TaskKind> {
    match value.as_str() {
        "forward" => Ok(TaskKind::Forward),
        "tunnel" => Ok(TaskKind::Tunnel),
        "syslog" => Ok(TaskKind::Syslog),
        "ostrace" | "os_trace" => Ok(TaskKind::OsTrace),
        "pcap" => Ok(TaskKind::Pcap),
        "notifications" => Ok(TaskKind::Notifications),
        "sysmontap" => Ok(TaskKind::Sysmontap),
        "accessibility_inspector" | "ax" => Ok(TaskKind::AccessibilityInspector),
        "devicestate" | "device_state" => Ok(TaskKind::DeviceState),
        "location_simulation" | "setlocationgpx" => Ok(TaskKind::LocationSimulation),
        "screenshot_stream" => Ok(TaskKind::ScreenshotStream),
        "ui_stream" => Ok(TaskKind::UiStream),
        "webinspector_cdp" | "web_inspector_cdp" => Ok(TaskKind::WebInspectorCdp),
        "webinspector_js_shell" | "web_inspector_js_shell" | "js_shell" => {
            Ok(TaskKind::WebInspectorJsShell)
        }
        "ui_runner" => Ok(TaskKind::UiRunner),
        "debug_proxy" => Ok(TaskKind::DebugProxy),
        "dproxy" => Ok(TaskKind::Dproxy),
        "wda" => Ok(TaskKind::Wda),
        "xctest" => Ok(TaskKind::XCTest),
        "app_process" => Ok(TaskKind::AppProcess),
        "other" => Ok(TaskKind::Other),
        _ => bail!("unknown task kind {value:?}"),
    }
}

fn parse_task_selector(value: &str) -> anyhow::Result<TaskSelector> {
    if value == "all" {
        Ok(TaskSelector::All)
    } else if let Ok(id) = value.parse::<Uuid>() {
        Ok(TaskSelector::Id(id))
    } else if let Some(kind) = value.strip_prefix("kind:") {
        Ok(TaskSelector::Kind(parse_task_kind(kind.to_owned())?))
    } else if let Some(port) = value.strip_prefix("port:") {
        Ok(TaskSelector::ForwardPort(port.parse()?))
    } else if let Some(udid) = value.strip_prefix("tunnel:") {
        Ok(TaskSelector::TunnelUdid(udid.to_owned()))
    } else {
        bail!("invalid task selector; use UUID, all, kind:<kind>, port:<port> or tunnel:<udid>")
    }
}

fn print_payload(
    payload: ResponsePayload,
    output: &OutputOptions,
    details: bool,
) -> anyhow::Result<()> {
    match payload {
        ResponsePayload::Command(result) => {
            for warning in result.warnings {
                eprintln!("warning: {warning}");
            }
            if let Some(value) = result.value {
                print_value(&value, output, result.text.as_deref())
            } else if let Some(text) = result.text {
                print_text(&text)
            } else {
                Ok(())
            }
        }
        ResponsePayload::PairRecord { record, .. } => print_value(&record, output, None),
        ResponsePayload::Devices(devices) if !details => {
            let udids: Vec<_> = devices
                .devices
                .into_iter()
                .map(|device| device.udid)
                .collect();
            if output.no_json {
                for udid in udids {
                    println!("{udid}");
                }
                Ok(())
            } else {
                print_value(&json!({ "deviceList": udids }), output, None)
            }
        }
        ResponsePayload::Ack { ref message } if output.no_json => {
            println!("{message}");
            Ok(())
        }
        ResponsePayload::Buid { ref buid } if output.no_json => {
            println!("{buid}");
            Ok(())
        }
        payload => print_value(&serde_json::to_value(payload)?, output, None),
    }
}

fn print_task_output_raw(payload: ResponsePayload) -> anyhow::Result<()> {
    let ResponsePayload::TaskOutput(output) = payload else {
        bail!("server returned an unexpected response to task output")
    };
    let bytes = BASE64
        .decode(output.bytes_base64)
        .context("server returned invalid base64 task output")?;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

fn print_text(text: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

fn print_value(value: &Value, output: &OutputOptions, plain: Option<&str>) -> anyhow::Result<()> {
    if output.no_json {
        if let Some(plain) = plain {
            println!("{plain}");
        } else if let Some(text) = value.as_str() {
            println!("{text}");
        } else {
            println!("{}", serde_json::to_string_pretty(value)?);
        }
    } else if output.pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

fn print_error(error: &anyhow::Error, output: &OutputOptions) {
    let client_error = error.downcast_ref::<ClientError>();
    let value = match client_error {
        Some(ClientError::Protocol(protocol)) => serde_json::to_value(protocol)
            .unwrap_or_else(|_| json!({ "code": "internal", "message": error.to_string() })),
        _ => json!({
            "code": "client_error",
            "message": format!("{error:#}")
        }),
    };
    if output.no_json {
        eprintln!(
            "error: {}",
            value["message"].as_str().unwrap_or("unknown error")
        );
    } else if output.pretty {
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&json!({ "error": value })).unwrap()
        );
    } else {
        eprintln!(
            "{}",
            serde_json::to_string(&json!({ "error": value })).unwrap()
        );
    }
}

fn exit_code(error: &anyhow::Error) -> u8 {
    match error.downcast_ref::<ClientError>() {
        Some(ClientError::Protocol(protocol))
            if protocol.code == rust_ios_protocol::ErrorCode::NotImplemented =>
        {
            2
        }
        Some(ClientError::Protocol(_)) => 3,
        Some(ClientError::Timeout | ClientError::Io(_) | ClientError::Spawn(_)) => 4,
        _ => 1,
    }
}

fn init_server_logging(
    verbose: bool,
    trace: bool,
    daemon_child: bool,
    log_path: &std::path::Path,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let default = if trace {
        "trace"
    } else if verbose {
        "debug"
    } else {
        "info"
    };
    let filter = std::env::var("RUST_IOS_LOG")
        .map(tracing_subscriber::EnvFilter::new)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    if daemon_child {
        let parent = log_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let file_name = log_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("server.log"));
        let appender = tracing_appender::rolling::never(parent, file_name);
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(writer)
            .try_init();
        Some(guard)
    } else {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_logging_writes_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.log");
        let guard = init_server_logging(false, false, true, &path).expect("guard");
        tracing::info!("daemon logging probe");
        drop(guard);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            text.contains("daemon logging probe"),
            "log contents: {text:?}"
        );
    }

    #[test]
    fn debug_app_parser_and_lldb_source_are_explicit() {
        assert!(
            debug_app_stop_at_entry(
                &["debug".into(), "Demo.app".into(), "--stop-at-entry".into(),]
            )
            .unwrap()
        );
        assert!(
            debug_app_stop_at_entry(&["debug".into(), "Demo.app".into(), "Other.app".into(),])
                .is_err()
        );
        assert!(
            debug_app_stop_at_entry(&["debug".into(), "Demo.app".into(), "--unknown".into()])
                .is_err()
        );

        let source = lldb_source(
            &DebugAppInfo {
                bundle_id: "com.example.Demo".into(),
                local_app_path: "C:\\Build\\Demo.app".into(),
                local_executable_path: "C:\\Build\\Demo.app\\Demo".into(),
                device_app_path: "/private/var/Demo.app".into(),
                device_executable_path: "/private/var/Demo.app/Demo".into(),
            },
            "127.0.0.1",
            43210,
            true,
        );
        assert!(source.contains("platform select remote-ios"));
        assert!(source.contains("target create -- \"C:/Build/Demo.app/Demo\""));
        assert!(source.contains("SBFileSpec(\"/private/var/Demo.app/Demo\")"));
        assert!(source.contains("process connect connect://127.0.0.1:43210"));
        assert!(source.contains("process launch --stop-at-entry"));
    }

    #[test]
    fn global_options_after_external_command_are_promoted() {
        let normalized = normalized_cli_args([
            OsString::from("ios"),
            OsString::from("info"),
            OsString::from("--server-home"),
            OsString::from("C:/isolated"),
            OsString::from("--udid=phone"),
        ]);
        let parsed = Cli::try_parse_from(normalized).unwrap();
        assert_eq!(parsed.server_home, Some(PathBuf::from("C:/isolated")));
        assert_eq!(parsed.udid.as_deref(), Some("phone"));
        let Some(TopCommand::External(arguments)) = parsed.command else {
            panic!("external command expected");
        };
        assert_eq!(arguments, vec![OsString::from("info")]);
    }

    #[test]
    fn explicit_server_home_builds_config_directly_from_the_absolute_path() {
        let parsed = Cli::try_parse_from([
            "ios",
            "--server-home=C:/rust-ios-explicit-root",
            "--server-addr=127.0.0.1:60241",
            "server",
            "status",
        ])
        .unwrap();
        let config = server_config(&parsed).unwrap();
        assert_eq!(
            config.paths.root,
            PathBuf::from("C:/rust-ios-explicit-root")
        );
        assert_eq!(config.listen_addr, "127.0.0.1:60241".parse().unwrap());

        let relative =
            Cli::try_parse_from(["ios", "--server-home=relative-root", "server", "status"])
                .unwrap();
        assert!(
            server_config(&relative)
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }

    #[test]
    fn remote_pairing_tunnel_and_manual_pair_build_explicit_requests() {
        let parsed = Cli::try_parse_from([
            "ios",
            "tunnel",
            "start",
            "--remote-pairing",
            "--udid",
            "phone",
        ])
        .unwrap();
        let Some(TopCommand::Tunnel { command }) = parsed.command else {
            panic!("tunnel command expected");
        };
        let Request::TunnelStart { spec } = tunnel_request(command, parsed.udid).unwrap() else {
            panic!("tunnel start request expected");
        };
        assert_eq!(spec.mode, TunnelMode::Userspace);
        assert_eq!(spec.transport, TunnelTransport::RemotePairingTcp);
        assert_eq!(spec.pair_record_path, "default");

        let parsed =
            Cli::try_parse_from(["ios", "tunnel", "pair", "--manual", "--udid", "phone"]).unwrap();
        let Some(TopCommand::Tunnel { command }) = parsed.command else {
            panic!("tunnel command expected");
        };
        let Request::TunnelPair { spec } = tunnel_request(command, parsed.udid).unwrap() else {
            panic!("tunnel pair request expected");
        };
        assert!(spec.manual);
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.pair_record_path, "default");
    }

    #[test]
    fn kernel_tunnel_resolves_wintun_path_and_requires_server_capability() {
        let wintun = if cfg!(windows) {
            "C:\\Windows\\System32\\wintun.dll"
        } else {
            "/opt/rust-ios/wintun.dll"
        };
        let parsed = Cli::try_parse_from([
            "ios",
            "tunnel",
            "start",
            "--udid",
            "phone",
            "--interface-name",
            "rust-ios-phone",
            "--wintun-path",
            wintun,
        ])
        .unwrap();
        let Some(TopCommand::Tunnel { command }) = parsed.command else {
            panic!("tunnel command expected");
        };
        let request = tunnel_request(command, parsed.udid).unwrap();
        let Request::TunnelStart { spec } = &request else {
            panic!("tunnel start request expected");
        };
        assert_eq!(spec.mode, TunnelMode::Kernel);
        assert_eq!(
            spec.kernel_interface_name.as_deref(),
            Some("rust-ios-phone")
        );
        assert_eq!(spec.wintun_path.as_deref(), Some(wintun));
        assert_eq!(
            required_server_capability(&request),
            Some("windows_kernel_tunnel")
        );
    }

    #[test]
    fn userspace_tunnel_rejects_kernel_only_options() {
        let parsed = Cli::try_parse_from([
            "ios",
            "tunnel",
            "start",
            "--userspace",
            "--interface-name",
            "rust-ios-phone",
        ])
        .unwrap();
        let Some(TopCommand::Tunnel { command }) = parsed.command else {
            panic!("tunnel command expected");
        };
        let error = tunnel_request(command, parsed.udid).unwrap_err();
        assert!(error.to_string().contains("require kernel tunnel mode"));
    }

    #[test]
    fn parses_both_mapping_separators() {
        assert_eq!(parse_port_mapping("8100:8101").unwrap(), (8100, 8101));
        assert_eq!(parse_port_mapping("8100=8101").unwrap(), (8100, 8101));
    }

    #[test]
    fn legacy_forward_syntax_builds_persistent_request() {
        let request = forward_request(
            ForwardArgs {
                values: vec!["8100".into(), "8101".into()],
                mappings: vec![],
                bind: "127.0.0.1".into(),
                network: false,
                usb: false,
            },
            Some("phone".into()),
        )
        .unwrap();
        let Request::ForwardStart { specs } = request else {
            panic!("expected forward start");
        };
        assert_eq!(specs[0].udid.as_deref(), Some("phone"));
        assert_eq!(specs[0].device_port, 8101);
    }

    #[test]
    fn syslog_command_builds_server_owned_stream_request() {
        let parsed = Cli::try_parse_from([
            "ios",
            "syslog",
            "--parse",
            "--udid",
            "phone",
            "--server-home",
            "C:/isolated",
        ])
        .unwrap();
        assert_eq!(parsed.udid.as_deref(), Some("phone"));
        let Some(TopCommand::Syslog(arguments)) = parsed.command else {
            panic!("syslog command expected");
        };
        let Request::StreamStart { spec } = stream_request(
            TaskKind::Syslog,
            parsed.udid,
            arguments.output,
            if arguments.parse {
                StreamFormat::JsonLines
            } else {
                StreamFormat::Raw
            },
            None,
            None,
            None,
        )
        .unwrap() else {
            panic!("stream start request expected");
        };
        assert_eq!(spec.kind, TaskKind::Syslog);
        assert_eq!(spec.format, StreamFormat::JsonLines);
        assert_eq!(spec.udid.as_deref(), Some("phone"));
    }

    #[test]
    fn ostrace_filters_build_typed_server_owned_options() {
        let parsed = Cli::try_parse_from([
            "ios",
            "ostrace",
            "--level=Error, fault,ERROR",
            "--subsystem=com.example",
            "--match=timeout",
            "--exclude=ignored",
            "--follow",
            "--udid=phone",
        ])
        .unwrap();
        let Some(TopCommand::Ostrace(arguments)) = parsed.command else {
            panic!("ostrace command expected");
        };
        let options = os_trace_options(&arguments).unwrap();
        assert_eq!(options.levels, [OsTraceLevel::Error, OsTraceLevel::Fault]);
        assert_eq!(options.subsystem_contains.as_deref(), Some("com.example"));
        assert_eq!(options.message_contains.as_deref(), Some("timeout"));
        assert_eq!(options.message_excludes.as_deref(), Some("ignored"));
        assert!(options.follow);
        assert!(parse_os_trace_levels(Some("info,bogus")).is_err());
    }

    #[test]
    fn gpx_command_builds_a_finite_location_task_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("route.gpx");
        std::fs::write(
            &path,
            "<gpx><trk><trkseg><trkpt lat=\"1\" lon=\"2\"><time>2026-07-16T00:00:00Z</time></trkpt><trkpt lat=\"3\" lon=\"4\"><time>2026-07-16T00:00:01Z</time></trkpt></trkseg></trk></gpx>",
        )
        .unwrap();
        let Request::LocationSimulationStart { spec } = external_stream_request(
            &[
                "setlocationgpx".into(),
                format!("--gpxfilepath={}", path.display()),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("location simulation request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.points.len(), 2);
        assert_eq!(spec.points[1].delay_before_ms, 1_000);
        assert!(std::path::Path::new(&spec.source_path).is_absolute());
        assert_eq!(spec.output_path, None);

        assert!(
            external_stream_request(
                &[
                    "setlocationgpx".into(),
                    format!("--gpxfilepath={}", path.display()),
                    format!("--gpxfilepath={}", path.display()),
                ],
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn dvt_stream_commands_build_server_owned_requests() {
        let Request::AccessibilityInspectorStart { spec } =
            external_stream_request(&["ax".into(), "--font=17.5".into()], Some("phone".into()))
                .unwrap()
                .unwrap()
        else {
            panic!("Accessibility inspector request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.font_size, Some(17.5));

        let Request::StreamStart { spec } = external_stream_request(
            &["instruments".into(), "notifications".into()],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("notifications stream request expected");
        };
        assert_eq!(spec.kind, TaskKind::Notifications);
        assert_eq!(spec.format, StreamFormat::JsonLines);

        let Request::StreamStart { spec } =
            external_stream_request(&["sysmontap".into()], Some("phone".into()))
                .unwrap()
                .unwrap()
        else {
            panic!("sysmontap stream request expected");
        };
        assert_eq!(spec.kind, TaskKind::Sysmontap);
        assert_eq!(spec.udid.as_deref(), Some("phone"));

        let Request::ScreenshotStreamStart { spec } = external_stream_request(
            &["screenshot".into(), "--stream".into(), "--port=0".into()],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("screenshot stream request expected");
        };
        assert_eq!(spec.bind_host, "127.0.0.1");
        assert_eq!(spec.port, 0);
        assert_eq!(spec.interval_ms, 100);

        let Request::DebugProxyStart { spec } = external_stream_request(
            &[
                "debug".into(),
                "proxy".into(),
                "--bind=127.0.0.1".into(),
                "--port=0".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("debug proxy request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.port, 0);
    }

    #[test]
    fn dproxy_builds_typed_persistent_request_and_rejects_duplicates() {
        let Request::DproxyStart { spec } = external_stream_request(
            &[
                "dproxy".into(),
                "--mode=usbmuxd".into(),
                "--binary".into(),
                "--bind=127.0.0.1".into(),
                "--port=0".into(),
                "--upstream=127.0.0.1:27016".into(),
                "--output=dproxy-capture.ndjson".into(),
            ],
            None,
        )
        .unwrap()
        .unwrap() else {
            panic!("dproxy request expected");
        };
        assert_eq!(spec.mode, DproxyMode::Usbmuxd);
        assert!(spec.binary_only);
        assert_eq!(spec.bind_host, "127.0.0.1");
        assert_eq!(spec.port, 0);
        assert_eq!(spec.upstream_address, "127.0.0.1:27016");
        assert!(PathBuf::from(spec.output_path.as_ref().unwrap()).is_absolute());
        assert_eq!(
            required_server_capability(&Request::DproxyStart { spec: spec.clone() }),
            Some("persistent_dproxy")
        );
        assert_eq!(parse_task_kind("dproxy".into()).unwrap(), TaskKind::Dproxy);

        for arguments in [
            vec![
                "dproxy".into(),
                "--mode=all".into(),
                "--mode=usbmuxd".into(),
            ],
            vec!["dproxy".into(), "--binary".into(), "--binary".into()],
            vec!["dproxy".into(), "--mode=invalid".into()],
            vec!["dproxy".into(), "--unknown".into()],
        ] {
            assert!(external_stream_request(&arguments, None).is_err());
        }

        let Request::DproxyStart { spec } = external_stream_request(
            &[
                "dproxy".into(),
                "--mode=utun".into(),
                "--iface=utun9".into(),
            ],
            None,
        )
        .unwrap()
        .unwrap() else {
            panic!("typed utun request expected");
        };
        assert_eq!(spec.mode, DproxyMode::Utun);
        assert_eq!(spec.interface.as_deref(), Some("utun9"));
    }

    #[test]
    fn accessibility_inspector_rejects_invalid_font_and_preserves_audit() {
        assert!(
            external_stream_request(&["ax".into(), "--font=0".into()], None)
                .unwrap_err()
                .to_string()
                .contains("greater than zero")
        );
        assert!(
            external_stream_request(&["ax".into(), "audit".into()], None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ui_stream_builds_server_owned_request_and_rejects_wda_h264() {
        let request = external_stream_request(
            &[
                "ui".into(),
                "stream".into(),
                "mjpeg".into(),
                "--driver=auto".into(),
                "--wda-url=http://127.0.0.1:18100".into(),
                "--devicekit-url".into(),
                "http://127.0.0.1:12004".into(),
                "--fps=30".into(),
                "--quality".into(),
                "80".into(),
                "--scale=0.5".into(),
                "--bitrate=4000000".into(),
                "--output=ui-stream.mjpeg".into(),
            ],
            Some("ignored-phone".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            required_server_capability(&request),
            Some("persistent_ui_stream")
        );
        let Request::UiStreamStart { spec } = request else {
            panic!("UI stream start request expected");
        };
        assert_eq!(spec.driver, UiAutomationDriver::Auto);
        assert_eq!(spec.format, UiStreamFormat::Mjpeg);
        assert_eq!(spec.fps, Some(30));
        assert_eq!(spec.quality, Some(80));
        assert_eq!(spec.scale, Some(0.5));
        assert_eq!(spec.bitrate, Some(4_000_000));
        assert!(PathBuf::from(spec.output_path.as_ref().unwrap()).is_absolute());

        let error = external_stream_request(
            &[
                "ui".into(),
                "stream".into(),
                "h264".into(),
                "--driver=wda".into(),
            ],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("supports mjpeg only"));
    }

    #[test]
    fn ui_run_builds_server_owned_wda_and_devicekit_requests() {
        let request = external_stream_request(
            &[
                "ui".into(),
                "run".into(),
                "wda".into(),
                "--bundleid=com.example.WDA".into(),
                "--test-runner-bundleid".into(),
                "com.example.WDARunner.xctrunner".into(),
                "--xctest-config=WebDriverAgentRunner.xctest".into(),
                "--host-port=0".into(),
                "--log-output=ui-runner.jsonl".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            required_server_capability(&request),
            Some("persistent_ui_runner")
        );
        let Request::UiRunnerStart { spec } = request else {
            panic!("UI runner start request expected");
        };
        assert_eq!(spec.target, UiRunnerTarget::Wda);
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.runner_bundle_id, "com.example.WDARunner.xctrunner");
        assert_eq!(spec.target_bundle_id, "com.example.WDA");
        assert_eq!(spec.host_port, 0);
        assert_eq!(spec.device_port, 8100);
        assert_eq!(spec.health_path, "/status");
        assert!(PathBuf::from(spec.output_path.unwrap()).is_absolute());

        let Request::UiRunnerStart { spec } =
            external_stream_request(&["ui".into(), "run".into(), "devicekit".into()], None)
                .unwrap()
                .unwrap()
        else {
            panic!("DeviceKit runner request expected");
        };
        assert_eq!(spec.target, UiRunnerTarget::DeviceKit);
        assert_eq!(
            spec.runner_bundle_id,
            "com.deviceboxhq.goios.devicekit.runner"
        );
        assert_eq!(spec.host_port, 12004);
        assert_eq!(spec.device_port, 12004);
        assert_eq!(spec.health_path, "/health");
        assert_eq!(spec.xctest_config, "devicekit-iosUITests.xctest");
    }

    #[test]
    fn ui_run_rejects_invalid_target_and_duplicate_options() {
        let error = external_stream_request(&["ui".into(), "run".into(), "unknown".into()], None)
            .unwrap_err();
        assert!(error.to_string().contains("must be wda or devicekit"));

        let error = external_stream_request(
            &[
                "ui".into(),
                "run".into(),
                "wda".into(),
                "--host-port=8100".into(),
                "--host-port=8101".into(),
            ],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("provided more than once"));
    }

    #[test]
    fn webinspector_cdp_builds_server_owned_request() {
        let Request::WebInspectorCdpStart { spec } = external_stream_request(
            &[
                "webinspector".into(),
                "cdp".into(),
                "--host=0.0.0.0".into(),
                "--port".into(),
                "0".into(),
                "--timeout=2.5".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("WebInspector CDP request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.bind_host, "0.0.0.0");
        assert_eq!(spec.port, 0);
        assert_eq!(spec.startup_timeout_ms, 2_500);

        let error = external_stream_request(
            &["webinspector".into(), "cdp".into(), "--timeout=0".into()],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("between 1 and 120 seconds"));
    }

    #[test]
    fn webinspector_js_shell_builds_interactive_task_spec() {
        let spec = webinspector_js_shell_spec(
            &[
                "webinspector".into(),
                "js-shell".into(),
                "https://example.com/path".into(),
                "--bundle-id=com.apple.mobilesafari".into(),
                "--open-safari".into(),
                "--timeout=2.5".into(),
            ],
            Some("phone".into()),
        )
        .unwrap();
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.url.as_deref(), Some("https://example.com/path"));
        assert_eq!(spec.bundle_id.as_deref(), Some("com.apple.mobilesafari"));
        assert!(spec.open_safari);
        assert_eq!(spec.command_timeout_ms, 2_500);
        assert_eq!(
            required_server_capability(&Request::WebInspectorJsShellStart { spec }),
            Some("interactive_webinspector_js_shell")
        );

        let error = webinspector_js_shell_spec(
            &[
                "webinspector".into(),
                "js-shell".into(),
                "first".into(),
                "second".into(),
            ],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("at most one URL"));
    }

    #[test]
    fn runwda_builds_server_owned_task_request() {
        let Request::WdaStart { spec } = external_stream_request(
            &[
                "runwda".into(),
                "--bundleid=com.example.WebDriverAgentRunner.xctrunner".into(),
                "--arg".into(),
                "--disable-keyboard".into(),
                "--env=USE_PORT=8100".into(),
                "--env".into(),
                "MJPEG_SERVER_PORT=9100".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("WDA start request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(
            spec.runner_bundle_id,
            "com.example.WebDriverAgentRunner.xctrunner"
        );
        assert_eq!(spec.target_bundle_id, Some(spec.runner_bundle_id.clone()));
        assert_eq!(spec.arguments, ["--disable-keyboard"]);
        assert_eq!(spec.environment["USE_PORT"], "8100");
        assert_eq!(spec.environment["MJPEG_SERVER_PORT"], "9100");
        assert!(spec.output_path.is_none());
        assert_eq!(
            required_server_capability(&Request::WdaStart { spec: spec.clone() }),
            Some("wda_event_capture")
        );
    }

    #[test]
    fn runwda_log_output_resolves_client_path_and_dash_uses_server_capture() {
        let Request::WdaStart { spec } = external_stream_request(
            &[
                "runwda".into(),
                "--log-output".into(),
                "wda-events.log".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("WDA start request expected");
        };
        assert!(PathBuf::from(spec.output_path.unwrap()).is_absolute());

        let Request::WdaStart { spec } = external_stream_request(
            &["runwda".into(), "--log-output=-".into()],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("WDA start request expected");
        };
        assert!(spec.output_path.is_none());

        let error = external_stream_request(
            &["runwda".into(), "--log-output=".into()],
            Some("phone".into()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--log-output cannot be empty"));
    }

    #[test]
    fn launch_wait_builds_finite_app_process_request() {
        let Request::AppLaunchStart { spec } = external_stream_request(
            &[
                "launch".into(),
                "com.example.Demo".into(),
                "--wait".into(),
                "--arg=--first".into(),
                "--arg".into(),
                "second".into(),
                "--env=FEATURE=true".into(),
                "--env".into(),
                "TOKEN=a=b".into(),
                "--kill-existing".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("app launch start request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.bundle_id, "com.example.Demo");
        assert_eq!(spec.arguments, ["--first", "second"]);
        assert_eq!(spec.environment["FEATURE"], "true");
        assert_eq!(spec.environment["TOKEN"], "a=b");
        assert!(spec.kill_existing);
        assert!(spec.output_path.is_none());
    }

    #[test]
    fn launch_without_wait_keeps_existing_execute_path() {
        assert!(
            external_stream_request(
                &["launch".into(), "com.example.Demo".into()],
                Some("phone".into()),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn runtest_builds_finite_xctest_task_request() {
        let Request::XCTestStart { spec } = external_stream_request(
            &[
                "runtest".into(),
                "--bundle-id=com.example.Demo".into(),
                "--test-runner-bundle-id".into(),
                "com.example.DemoUITests.xctrunner".into(),
                "--xctest-config=DemoUITests.xctest".into(),
                "--test-to-run=DemoUITests/testOne".into(),
                "--test-to-skip".into(),
                "DemoUITests/testTwo".into(),
                "--env=FEATURE=true".into(),
                "--log-output=xctest-events.jsonl".into(),
            ],
            Some("phone".into()),
        )
        .unwrap()
        .unwrap() else {
            panic!("XCTest start request expected");
        };
        assert_eq!(spec.udid.as_deref(), Some("phone"));
        assert_eq!(spec.targets.len(), 1);
        assert_eq!(spec.targets[0].name, "DemoUITests");
        assert_eq!(
            spec.targets[0].runner_bundle_id,
            "com.example.DemoUITests.xctrunner"
        );
        assert_eq!(
            spec.targets[0].target_bundle_id.as_deref(),
            Some("com.example.Demo")
        );
        assert_eq!(spec.targets[0].tests_to_run, ["DemoUITests/testOne"]);
        assert_eq!(spec.targets[0].tests_to_skip, ["DemoUITests/testTwo"]);
        assert_eq!(
            spec.targets[0].environment["FEATURE"],
            XCTestEnvironmentValue::String("true".into())
        );
        assert!(spec.targets[0].is_ui_test);
        assert!(PathBuf::from(spec.output_path.as_ref().unwrap()).is_absolute());
        assert_eq!(
            required_server_capability(&Request::XCTestStart { spec: spec.clone() }),
            Some("xctest_unit_filters")
        );
    }

    #[test]
    fn runtest_xctest_builds_unit_target_and_requires_bundle_name() {
        let Request::XCTestStart { spec } = external_stream_request(
            &[
                "runtest".into(),
                "--test-runner-bundle-id=com.example.UnitTests".into(),
                "--xctest-config=UnitTests.xctest".into(),
                "--xctest".into(),
            ],
            None,
        )
        .unwrap()
        .unwrap() else {
            panic!("XCTest start request expected");
        };
        assert!(!spec.targets[0].is_ui_test);
        assert_eq!(
            required_server_capability(&Request::XCTestStart { spec: spec.clone() }),
            Some("xctest_unit_filters")
        );

        let error = external_stream_request(
            &[
                "runtest".into(),
                "--test-runner-bundle-id=com.example.UnitTests".into(),
                "--xctest".into(),
            ],
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--xctest-config with a non-empty .xctest bundle name")
        );

        let error = external_stream_request(
            &[
                "runtest".into(),
                "--test-runner-bundle-id=com.example.UnitTests".into(),
                "--xctest-config=UnitTests".into(),
                "--xctest".into(),
            ],
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-empty .xctest bundle name"));

        for arguments in [
            vec![
                "runtest".into(),
                "--test-runner-bundle-id=com.example.UnitTests".into(),
                "--xctest-config=.xctest".into(),
                "--xctest".into(),
            ],
            vec![
                "runtest".into(),
                "--test-runner-bundle-id=com.example.UnitTests".into(),
                "--xctest-config=UnitTests.xctest".into(),
                "--bundle-id=com.example.App".into(),
                "--xctest".into(),
            ],
        ] {
            assert!(external_stream_request(&arguments, None).is_err());
        }
    }

    #[test]
    fn external_help_is_intercepted_before_device_dispatch() {
        assert_eq!(
            external_help_request(&[
                OsString::from("ios"),
                OsString::from("info"),
                OsString::from("--help"),
            ]),
            Some("info".into())
        );
        assert_eq!(
            external_help_request(&[
                OsString::from("ios"),
                OsString::from("--udid"),
                OsString::from("phone"),
                OsString::from("apps"),
                OsString::from("-h"),
            ]),
            Some("apps".into())
        );
        assert_eq!(
            external_help_request(&[
                OsString::from("ios"),
                OsString::from("list"),
                OsString::from("--help"),
            ]),
            None
        );
    }

    #[test]
    fn help_accepts_legacy_command_path() {
        let parsed = Cli::try_parse_from(["ios", "help", "file", "ls"]).unwrap();
        let Some(TopCommand::Help { command }) = parsed.command else {
            panic!("help command expected");
        };
        assert_eq!(command, ["file", "ls"]);
    }
}
