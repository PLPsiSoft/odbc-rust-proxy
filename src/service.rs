// Copyright (C) 2026 PSI SOFT Sp. z o.o.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//         http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

pub const SERVICE_NAME: &str = "PsiOdbcRustProxy";
pub const SERVICE_DISPLAY_NAME: &str = "PSI: ODBC Rust Proxy (32-bit Bridge)";
pub const SERVICE_DESCRIPTION: &str =
    "Lekki serwer proxy HTTP mostkujący 32-bitowe źródła ODBC dla 64-bitowych aplikacji Java/JDBC.";

define_windows_service!(ffi_service_main, my_service_main);

/// Punkt wejścia dla Windows Service Control Manager (SCM).
/// Uruchamia dispatcher usług Win32 dla usługi SERVICE_NAME.
pub fn run_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

/// Główna funkcja wykonawcza usługi wywoływana przez SCM.
fn my_service_main(_arguments: Vec<OsString>) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let shutdown_tx = std::sync::Mutex::new(Some(shutdown_tx));

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Ok(mut lock) = shutdown_tx.lock() {
                    if let Some(tx) = lock.take() {
                        let _ = tx.send(());
                    }
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("Błąd rejestracji procedury obsługi usługi Windows: {:?}", e);
            return;
        }
    };

    // Zgłoszenie stanu Running do SCM
    let running_status = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    if let Err(e) = status_handle.set_service_status(running_status) {
        eprintln!("Błąd ustawiania statusu Running w SCM: {:?}", e);
        return;
    }

    // Inicjalizacja wielowątkowego środowiska Tokio
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Błąd tworzenia środowiska Tokio: {:?}", e);
            let _ = status_handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(1),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            });
            return;
        }
    };

    // Uruchomienie serwera HTTP Axum z obsługą graceful shutdown na kanale shutdown_rx
    let server_result = rt.block_on(crate::run_server(shutdown_rx));

    // Zgłoszenie stanu Stopped do SCM
    let exit_code = match server_result {
        Ok(()) => ServiceExitCode::NO_ERROR,
        Err(e) => {
            eprintln!("Błąd wykonania serwera proxy: {:?}", e);
            ServiceExitCode::Win32(1)
        }
    };

    let stopped_status = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let _ = status_handle.set_service_status(stopped_status);
}

/// Instaluje aplikację jako natywną usługę Windows z autostartem.
pub fn install_service() -> anyhow::Result<()> {
    let exe_path: PathBuf = std::env::current_exe()?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| map_winapi_error(e, "Błąd łączenia z Service Control Manager"))?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path.clone(),
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // uruchamianie na koncie LocalSystem
        account_password: None,
    };

    let service = manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| map_winapi_error(e, "Błąd tworzenia usługi Windows"))?;

    let _ = service.set_description(SERVICE_DESCRIPTION);

    println!("--------------------------------------------------");
    println!("SUKCES: Usługa '{}' została pomyślnie zainstalowana!", SERVICE_NAME);
    println!("Ścieżka pliku binarnego: {}", exe_path.display());
    println!("Typ uruchamiania: Automatyczny (AutoStart)");
    println!("--------------------------------------------------");
    println!("Możesz uruchomić usługę poleceniem:");
    println!("    odbc-rust-proxy.exe start");
    println!("lub w konsoli usług 'services.msc'.");

    Ok(())
}

/// Bezpiecznie zatrzymuje i deinstaluje usługę Windows z systemu.
pub fn uninstall_service() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )
    .map_err(|e| map_winapi_error(e, "Błąd łączenia z Service Control Manager"))?;

    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
        )
        .map_err(|e| map_winapi_error(e, "Błąd otwierania usługi"))?;

    // Jeśli usługa jest uruchomiona, najpierw ją zatrzymaj
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            println!("Zatrzymywanie działającej usługi '{}'...", SERVICE_NAME);
            let _ = service.stop();
            // Poczekaj chwilę na zatrzymanie procesu
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(s) = service.query_status() {
                    if s.current_state == ServiceState::Stopped {
                        break;
                    }
                }
            }
        }
    }

    service
        .delete()
        .map_err(|e| map_winapi_error(e, "Błąd usuwania usługi z bazy SCM"))?;

    println!("SUKCES: Usługa '{}' została pomyślnie odinstalowana z systemu.", SERVICE_NAME);
    Ok(())
}

/// Uruchamia zarejestrowaną usługę Windows.
pub fn start_service() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )
    .map_err(|e| map_winapi_error(e, "Błąd łączenia z Service Control Manager"))?;

    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| map_winapi_error(e, "Błąd otwierania usługi"))?;

    let status = service.query_status()?;
    if status.current_state == ServiceState::Running {
        println!("Usługa '{}' już jest uruchomiona.", SERVICE_NAME);
        return Ok(());
    }

    println!("Uruchamianie usługi '{}'...", SERVICE_NAME);
    service
        .start::<&OsStr>(&[])
        .map_err(|e| map_winapi_error(e, "Błąd podczas wywołania start usługi"))?;

    // Oczekiwanie na zgłoszenie stanu Running
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(500));
        let s = service.query_status()?;
        if s.current_state == ServiceState::Running {
            println!("SUKCES: Usługa '{}' została pomyślnie uruchomiona.", SERVICE_NAME);
            return Ok(());
        }
    }

    println!("Polecenie start zostało przekazane. Bieżący status: {:?}", service.query_status()?.current_state);
    Ok(())
}

/// Zatrzymuje działającą usługę Windows.
pub fn stop_service() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )
    .map_err(|e| map_winapi_error(e, "Błąd łączenia z Service Control Manager"))?;

    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|e| map_winapi_error(e, "Błąd otwierania usługi"))?;

    let status = service.query_status()?;
    if status.current_state == ServiceState::Stopped {
        println!("Usługa '{}' jest już zatrzymana.", SERVICE_NAME);
        return Ok(());
    }

    println!("Zatrzymywanie usługi '{}'...", SERVICE_NAME);
    service
        .stop()
        .map_err(|e| map_winapi_error(e, "Błąd zatrzymywania usługi"))?;

    // Oczekiwanie na zgłoszenie stanu Stopped
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(500));
        let s = service.query_status()?;
        if s.current_state == ServiceState::Stopped {
            println!("SUKCES: Usługa '{}' została zatrzymana.", SERVICE_NAME);
            return Ok(());
        }
    }

    println!("Polecenie zatrzymania zostało przekazane. Bieżący status: {:?}", service.query_status()?.current_state);
    Ok(())
}

/// Sprawdza bieżący status usługi Windows w systemie.
pub fn query_service_status() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )
    .map_err(|e| map_winapi_error(e, "Błąd łączenia z Service Control Manager"))?;

    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(ref io_err)) if io_err.raw_os_error() == Some(1060) => {
            println!("========================================");
            println!("Usługa:        {} ({})", SERVICE_NAME, SERVICE_DISPLAY_NAME);
            println!("Stan:          Nie zainstalowana w systemie");
            println!("========================================");
            println!("Aby zainstalować usługę, uruchom terminal jako Administrator i wpisz:");
            println!("    odbc-rust-proxy.exe install");
            return Ok(());
        }
        Err(e) => return Err(map_winapi_error(e, "Błąd otwierania usługi")),
    };

    let status = service
        .query_status()
        .map_err(|e| map_winapi_error(e, "Błąd pobierania statusu usługi"))?;

    let state_str = match status.current_state {
        ServiceState::Stopped => "Zatrzymana (Stopped)",
        ServiceState::StartPending => "Uruchamianie... (StartPending)",
        ServiceState::StopPending => "Zatrzymywanie... (StopPending)",
        ServiceState::Running => "Działa (Running)",
        ServiceState::ContinuePending => "Wznawianie... (ContinuePending)",
        ServiceState::PausePending => "Wstrzymywanie... (PausePending)",
        ServiceState::Paused => "Wstrzymana (Paused)",
    };

    println!("========================================");
    println!("Usługa:        {} ({})", SERVICE_NAME, SERVICE_DISPLAY_NAME);
    println!("Stan:          {}", state_str);
    if let Some(pid) = status.process_id {
        println!("PID procesu:   {}", pid);
    }
    println!("========================================");

    Ok(())
}

fn map_winapi_error(err: windows_service::Error, context: &str) -> anyhow::Error {
    if let windows_service::Error::Winapi(ref io_err) = err {
        match io_err.raw_os_error() {
            Some(5) => anyhow::anyhow!(
                "{}: Odmowa dostępu (brak uprawnień Administratora). Uruchom terminal jako Administrator!",
                context
            ),
            Some(1060) => anyhow::anyhow!(
                "{}: Usługa '{}' nie jest zainstalowana w systemie. Użyj: odbc-rust-proxy.exe install",
                context,
                SERVICE_NAME
            ),
            Some(1062) => anyhow::anyhow!(
                "{}: Usługa '{}' nie jest aktualnie uruchomiona.",
                context,
                SERVICE_NAME
            ),
            Some(1056) => anyhow::anyhow!(
                "{}: Usługa '{}' jest już uruchomiona.",
                context,
                SERVICE_NAME
            ),
            Some(1072) => anyhow::anyhow!(
                "{}: Usługa '{}' jest oznaczona do usunięcia. Zrestartuj system lub zamknij programy mające otwarty uchwyt usługi.",
                context,
                SERVICE_NAME
            ),
            _ => anyhow::anyhow!("{}: {} (kod błędu OS: {:?})", context, io_err, io_err.raw_os_error()),
        }
    } else {
        anyhow::anyhow!("{}: {}", context, err)
    }
}
