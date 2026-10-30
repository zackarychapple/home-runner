use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::config::Config;
use crate::{daemon, logging, paths};

const SERVICE_NAME: &str = "home-runner";
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Called as `home-runner.exe service --config <path>` by the service manager.
pub fn run(config_path: PathBuf) -> Result<()> {
    let _ = CONFIG_PATH.set(config_path);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context(
        "not started by the Windows service manager; use `home-runner run` in a console",
    )?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    let path = CONFIG_PATH
        .get()
        .cloned()
        .unwrap_or_else(paths::default_config_path);
    let config = Config::load(&path);
    let log_dir = match &config {
        Ok(config) => config.log_dir(),
        Err(_) => paths::default_data_dir().join("logs"),
    };
    let _log = logging::init(Some(&log_dir));
    if let Err(e) = run_service(config) {
        tracing::error!("service stopped with an error: {e:#}");
    }
}

fn run_service(config: Result<Config>) -> Result<()> {
    let shutdown = CancellationToken::new();
    let on_stop = shutdown.clone();
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            on_stop.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let result = (|| {
        let config = config?;
        set_status(handle, ServiceState::Running, 0, Duration::ZERO)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let stopping = shutdown.clone();
            tokio::spawn(async move {
                stopping.cancelled().await;
                // Tell the SCM we're draining so it waits for runners to be torn down.
                let _ = set_status(
                    handle,
                    ServiceState::StopPending,
                    0,
                    Duration::from_secs(120),
                );
            });
            daemon::run(config, shutdown).await
        })
    })();

    // A non-zero exit triggers the restart-on-failure actions configured at install time.
    set_status(
        handle,
        ServiceState::Stopped,
        u32::from(result.is_err()),
        Duration::ZERO,
    )?;
    result
}

fn set_status(
    handle: ServiceStatusHandle,
    state: ServiceState,
    exit_code: u32,
    wait_hint: Duration,
) -> Result<()> {
    let controls_accepted = match state {
        ServiceState::Running => ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        _ => ServiceControlAccept::empty(),
    };
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(exit_code),
        checkpoint: 0,
        wait_hint,
        process_id: None,
    })?;
    Ok(())
}

pub fn install(
    exe: &Path,
    config: &Path,
    account: Option<String>,
    password: Option<String>,
) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("connecting to the service manager (run this from an Administrator terminal)")?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "home-runner (GitHub Actions runners)".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![
            "service".into(),
            "--config".into(),
            config.as_os_str().to_owned(),
        ],
        dependencies: vec![],
        account_name: account.map(OsString::from),
        account_password: password.map(OsString::from),
    };
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(existing) => {
            stop_and_wait(&existing)?;
            existing.change_config(&info)?;
            existing
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description("Keeps ephemeral GitHub Actions runners online on this machine")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(
            [10, 30, 60]
                .into_iter()
                .map(|secs| ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay: Duration::from_secs(secs),
                })
                .collect(),
        ),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.start(&[] as &[&OsStr])?;
    println!("installed and started Windows service {SERVICE_NAME:?}");
    println!("logs: {}", paths::default_data_dir().join("logs").display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("connecting to the service manager (run this from an Administrator terminal)")?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    stop_and_wait(&service)?;
    service.delete()?;
    println!("removed Windows service {SERVICE_NAME:?}");
    Ok(())
}

/// Restarts the service so it picks up config changes. Needs an Administrator.
pub fn restart() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("connecting to the service manager")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START,
        )
        .context("opening the home-runner service (restarting it needs an Administrator)")?;
    stop_and_wait(&service)?;
    service.start(&[] as &[&OsStr])?;
    Ok(())
}

fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    let _ = service.stop();
    let deadline = Instant::now() + Duration::from_secs(150);
    while service.query_status()?.current_state != ServiceState::Stopped {
        anyhow::ensure!(
            Instant::now() < deadline,
            "service did not stop within 150s"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
