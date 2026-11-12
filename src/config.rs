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

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Run,
    ServiceWorker,
    Install { forward_args: Vec<String> },
    Uninstall,
    Start,
    Stop,
    Status,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub idle_timeout_secs: u64,
}

impl Config {
    pub const DEFAULT_HOST: &'static str = "127.0.0.1";
    pub const DEFAULT_PORT: u16 = 8097;
    pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;
    pub const DEFAULT_CERT_FILENAME: &'static str = "cert.pem";
    pub const DEFAULT_KEY_FILENAME: &'static str = "key.pem";

    pub fn from_env_and_args() -> Result<(CliAction, Self), String> {
        let env_cfg = Self::from_env()?;
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::parse_cli_with_base(env_cfg, &args)
    }

    pub fn from_env() -> Result<Self, String> {
        let mut host = Self::DEFAULT_HOST.to_string();
        let mut port = Self::DEFAULT_PORT;

        if let Ok(addr) = std::env::var("ODBC_PROXY_ADDR").or_else(|_| std::env::var("BIND_ADDR")) {
            Self::apply_addr_str(&mut host, &mut port, &addr)?;
        }
        if let Ok(port_str) = std::env::var("ODBC_PROXY_PORT").or_else(|_| std::env::var("PORT")) {
            port = port_str
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("Nieprawidłowy port w zmiennej środowiskowej: '{}'", port_str))?;
        }

        let tls_enabled = match std::env::var("ODBC_TLS_ENABLED") {
            Ok(val) => Self::parse_bool(&val, "ODBC_TLS_ENABLED")?,
            Err(_) => true, // Opcja A: Fail-Closed (TLS włączony domyślnie)
        };

        let tls_cert = std::env::var("ODBC_TLS_CERT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let tls_key = std::env::var("ODBC_TLS_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let idle_timeout_secs = match std::env::var("ODBC_IDLE_TIMEOUT_SECS") {
            Ok(val) if !val.trim().is_empty() => val
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("Nieprawidłowa wartość ODBC_IDLE_TIMEOUT_SECS: '{}'", val))?,
            _ => Self::DEFAULT_IDLE_TIMEOUT_SECS,
        };

        Ok(Self {
            host,
            port,
            tls_enabled,
            tls_cert,
            tls_key,
            idle_timeout_secs,
        })
    }

    fn parse_bool(raw: &str, name: &str) -> Result<bool, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(format!(
                "Nieprawidłowa wartość logiczna dla {}: '{}' (oczekiwano true/false)",
                name, raw
            )),
        }
    }

    pub fn parse_cli_with_base(mut cfg: Self, args: &[String]) -> Result<(CliAction, Self), String> {
        if args.is_empty() {
            return Ok((CliAction::Run, cfg));
        }

        let (action, flag_args) = match args[0].as_str() {
            "--service" => (CliAction::ServiceWorker, &args[1..]),
            "run" => (CliAction::Run, &args[1..]),
            "install" => (
                CliAction::Install {
                    forward_args: args[1..].to_vec(),
                },
                &args[1..],
            ),
            "uninstall" => (CliAction::Uninstall, &args[1..]),
            "start" => (CliAction::Start, &args[1..]),
            "stop" => (CliAction::Stop, &args[1..]),
            "status" => (CliAction::Status, &args[1..]),
            "--help" | "-h" | "help" => return Ok((CliAction::Help, cfg)),
            first if first.starts_with('-') => (CliAction::Run, args),
            unknown => {
                return Err(format!(
                    "Nieznane polecenie: '{}'. Użyj --help, aby wyświetlić dostępne opcje.",
                    unknown
                ));
            }
        };

        // Dla uninstall/start/stop/status nie akceptujemy nadmiarowych argumentów
        match action {
            CliAction::Uninstall | CliAction::Start | CliAction::Stop | CliAction::Status
                if !flag_args.is_empty() =>
            {
                return Err(format!(
                    "Polecenie '{}' nie przyjmuje dodatkowych argumentów: {:?}",
                    args[0], flag_args
                ));
            }
            _ => {}
        }

        let wants_help = Self::apply_flags(&mut cfg, flag_args)?;
        if wants_help {
            return Ok((CliAction::Help, cfg));
        }

        if let CliAction::Install { .. } = action {
            let normalized = Self::normalize_install_args(flag_args)?;
            return Ok((
                CliAction::Install {
                    forward_args: normalized,
                },
                cfg,
            ));
        }

        Ok((action, cfg))
    }

    /// Normalizuje ścieżki --tls-cert i --tls-key do ścieżek bezwzględnych podczas `install`,
    /// ponieważ usługa Windows startuje w katalogu systemowym (System32/SysWOW64).
    fn normalize_install_args(args: &[String]) -> Result<Vec<String>, String> {
        let mut out = Vec::with_capacity(args.len());
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--tls-cert" || arg == "--tls-key" {
                out.push(arg.clone());
                i += 1;
                if i < args.len() {
                    out.push(Self::make_absolute_if_relative(&args[i])?);
                }
            } else if let Some(val) = arg.strip_prefix("--tls-cert=") {
                out.push(format!("--tls-cert={}", Self::make_absolute_if_relative(val)?));
            } else if let Some(val) = arg.strip_prefix("--tls-key=") {
                out.push(format!("--tls-key={}", Self::make_absolute_if_relative(val)?));
            } else {
                out.push(arg.clone());
            }
            i += 1;
        }
        Ok(out)
    }

    fn make_absolute_if_relative(raw: &str) -> Result<String, String> {
        let p = Path::new(raw);
        if p.is_absolute() {
            Ok(raw.to_string())
        } else {
            let cwd = std::env::current_dir()
                .map_err(|e| format!("Nie udało się odczytać bieżącego katalogu: {}", e))?;
            Ok(cwd.join(p).to_string_lossy().into_owned())
        }
    }

    fn apply_flags(cfg: &mut Self, args: &[String]) -> Result<bool, String> {
        let mut cli_no_tls = false;
        let mut cli_tls_path_given = false;
        let mut i = 0;

        while i < args.len() {
            let arg = &args[i];
            if arg == "-h" || arg == "--help" {
                return Ok(true);
            } else if arg == "--no-tls" {
                cli_no_tls = true;
                cfg.tls_enabled = false;
            } else if arg == "-b" || arg == "--bind" || arg == "-a" || arg == "--addr" {
                let val = Self::next_flag_value(arg, args, &mut i)?;
                Self::apply_addr_str(&mut cfg.host, &mut cfg.port, val)?;
            } else if let Some(val) = arg
                .strip_prefix("--bind=")
                .or_else(|| arg.strip_prefix("--addr="))
                .or_else(|| arg.strip_prefix("-b="))
                .or_else(|| arg.strip_prefix("-a="))
            {
                Self::apply_addr_str(&mut cfg.host, &mut cfg.port, val)?;
            } else if arg == "-p" || arg == "--port" {
                let val = Self::next_flag_value(arg, args, &mut i)?;
                cfg.port = Self::parse_port(val)?;
            } else if let Some(val) = arg.strip_prefix("--port=").or_else(|| arg.strip_prefix("-p=")) {
                cfg.port = Self::parse_port(val)?;
            } else if arg == "--tls-cert" {
                let val = Self::next_flag_value(arg, args, &mut i)?;
                cli_tls_path_given = true;
                cfg.tls_cert = Some(PathBuf::from(val));
            } else if let Some(val) = arg.strip_prefix("--tls-cert=") {
                cli_tls_path_given = true;
                cfg.tls_cert = Some(PathBuf::from(val));
            } else if arg == "--tls-key" {
                let val = Self::next_flag_value(arg, args, &mut i)?;
                cli_tls_path_given = true;
                cfg.tls_key = Some(PathBuf::from(val));
            } else if let Some(val) = arg.strip_prefix("--tls-key=") {
                cli_tls_path_given = true;
                cfg.tls_key = Some(PathBuf::from(val));
            } else {
                return Err(format!(
                    "Nieznany argument: '{}'. Użyj --help, aby wyświetlić dostępne opcje.",
                    arg
                ));
            }
            i += 1;
        }

        if cli_no_tls && cli_tls_path_given {
            return Err(
                "Flaga --no-tls nie może być łączona z parametrami --tls-cert lub --tls-key."
                    .to_string(),
            );
        }

        Ok(false)
    }

    fn next_flag_value<'a>(
        flag: &str,
        args: &'a [String],
        idx: &mut usize,
    ) -> Result<&'a str, String> {
        *idx += 1;
        if *idx >= args.len() {
            return Err(format!("Brak wartości po fladze '{}'", flag));
        }
        Ok(args[*idx].as_str())
    }

    fn parse_port(raw: &str) -> Result<u16, String> {
        raw.trim()
            .parse::<u16>()
            .map_err(|_| format!("Nieprawidłowy numer portu: '{}'", raw))
    }

    pub fn apply_addr_str(host: &mut String, port: &mut u16, addr_str: &str) -> Result<(), String> {
        let trimmed = addr_str.trim();
        if trimmed.is_empty() {
            return Err("Podano pusty adres".to_string());
        }

        // 1. Czy podano sam numer portu (np. "9000")
        if let Ok(p) = trimmed.parse::<u16>() {
            *port = p;
            return Ok(());
        }

        // 2. Czy to pełny SocketAddr (np. "127.0.0.1:8097" lub "[::1]:8097")
        if let Ok(sa) = trimmed.parse::<SocketAddr>() {
            *host = if sa.is_ipv6() {
                format!("[{}]", sa.ip())
            } else {
                sa.ip().to_string()
            };
            *port = sa.port();
            return Ok(());
        }

        // 3. Czy to sam adres IP (np. "0.0.0.0", "192.168.1.5", "::1")
        if let Ok(ip) = trimmed.parse::<IpAddr>() {
            *host = if ip.is_ipv6() {
                format!("[{}]", ip)
            } else {
                ip.to_string()
            };
            return Ok(());
        }

        // 4. Host i port rozdzielone dwukropkiem (np. "localhost:8097")
        if let Some((h, p_str)) = trimmed.rsplit_once(':') {
            if !h.is_empty() {
                *host = h.to_string();
            }
            if !p_str.is_empty() {
                *port = p_str
                    .parse::<u16>()
                    .map_err(|_| format!("Nieprawidłowy port w adresie '{}'", trimmed))?;
            }
            return Ok(());
        }

        // 5. Sam host (np. "localhost")
        *host = trimmed.to_string();
        Ok(())
    }

    pub fn socket_addr_str(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Rozwiązuje ścieżki certyfikatu (cert.pem) i klucza prywatnego (key.pem).
    /// W trybie domyślnym (Opcja A: Fail-Closed) brak pliku kończy się czytelnym błędem.
    pub fn resolve_tls_paths(&self) -> Result<(PathBuf, PathBuf), String> {
        let cert = Self::resolve_tls_file(
            self.tls_cert.as_deref(),
            Self::DEFAULT_CERT_FILENAME,
            "certyfikatu TLS",
        )?;
        let key = Self::resolve_tls_file(
            self.tls_key.as_deref(),
            Self::DEFAULT_KEY_FILENAME,
            "klucza prywatnego TLS",
        )?;
        Ok((cert, key))
    }

    fn resolve_tls_file(
        explicit: Option<&Path>,
        default_filename: &str,
        label: &str,
    ) -> Result<PathBuf, String> {
        if let Some(path) = explicit {
            if path.is_file() {
                return Ok(path.to_path_buf());
            }
            return Err(format!(
                "Nie znaleziono pliku {} pod wskazaną ścieżką: '{}'",
                label,
                path.display()
            ));
        }

        let mut checked = Vec::new();

        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let candidate = exe_dir.join(default_filename);
                if candidate.is_file() {
                    return Ok(candidate);
                }
                checked.push(candidate);
            }
        }

        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd.join(default_filename);
            if !checked.contains(&candidate) {
                if candidate.is_file() {
                    return Ok(candidate);
                }
                checked.push(candidate);
            }
        }

        let checked_list = checked
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");

        Err(format!(
            "BŁĄD TLS (Fail-Closed): Nie znaleziono wymaganego pliku {} ('{}').\nSprawdzono lokalizacje:\n{}\n\n\
            Aby wygenerować parę cert.pem + key.pem (ważność 30 lat, SAN localhost + 127.0.0.1), uruchom:\n\
            MSYS_NO_PATHCONV=1 openssl req -x509 -newkey rsa:2048 -nodes \\\n\
              -keyout key.pem -out cert.pem -days 10950 \\\n\
              -subj \"/CN=localhost\" \\\n\
              -addext \"subjectAltName=DNS:localhost,IP:127.0.0.1\"\n\n\
            Możesz też wskazać pliki flagami --tls-cert / --tls-key (lub zmiennymi ODBC_TLS_CERT / ODBC_TLS_KEY).\n\
            Do testów lokalnych bez szyfrowania użyj flagi --no-tls (lub ODBC_TLS_ENABLED=false).",
            label, default_filename, checked_list
        ))
    }

    pub fn print_help() {
        println!("ODBC Rust Proxy - 32-bit ODBC HTTP/HTTPS Bridge");
        println!("===============================================");
        println!("UŻYCIE:");
        println!("  odbc-rust-proxy.exe [POLECENIE] [OPCJE]");
        println!();
        println!("POLECENIA:");
        println!("  (brak) / run       Uruchamia serwer w konsoli (tryb Standalone, Ctrl+C zatrzymuje)");
        println!("  install            Instaluje program jako natywną usługę Windows (AutoStart)");
        println!("  uninstall          Zatrzymuje i odinstalowuje usługę Windows");
        println!("  start              Uruchamia zarejestrowaną usługę Windows");
        println!("  stop               Zatrzymuje usługę Windows");
        println!("  status             Sprawdza status usługi Windows");
        println!("  --service          Flaga wewnętrzna używana przez Windows Service Manager");
        println!("  --help, -h         Wyświetla ten komunikat pomocy");
        println!();
        println!("OPCJE SIECIOWE I TLS:");
        println!("  -b, --bind, -a, --addr <ADRES>  Adres i port nasłuchiwania (domyślnie: 127.0.0.1:8097)");
        println!("  -p, --port <PORT>               Port do nasłuchiwania (domyślnie: 8097)");
        println!("  --tls-cert <ŚCIEŻKA>            Ścieżka do pliku cert.pem (domyślnie obok .exe)");
        println!("  --tls-key <ŚCIEŻKA>             Ścieżka do pliku key.pem (domyślnie obok .exe)");
        println!("  --no-tls                        Wyłącza TLS i uruchamia czyste HTTP (tylko do testów)");
        println!();
        println!("ZMIENNE ŚRODOWISKOWE:");
        println!("  ODBC_PROXY_ADDR, BIND_ADDR      Adres i port nasłuchiwania (domyślnie: 127.0.0.1:8097)");
        println!("  ODBC_PROXY_PORT, PORT           Port nasłuchu");
        println!("  ODBC_TLS_ENABLED                true (domyślnie) lub false");
        println!("  ODBC_TLS_CERT                   Ścieżka do certyfikatu PEM");
        println!("  ODBC_TLS_KEY                    Ścieżka do klucza prywatnego PEM");
        println!("  ODBC_IDLE_TIMEOUT_SECS          Timeout bezczynności sesji w sekundach (domyślnie: 1800)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_test_cfg() -> Config {
        Config {
            host: Config::DEFAULT_HOST.to_string(),
            port: Config::DEFAULT_PORT,
            tls_enabled: true,
            tls_cert: None,
            tls_key: None,
            idle_timeout_secs: Config::DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }

    #[test]
    fn test_apply_addr_str_cases() {
        let mut host = "127.0.0.1".to_string();
        let mut port = 8097;

        Config::apply_addr_str(&mut host, &mut port, "9000").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 9000);

        Config::apply_addr_str(&mut host, &mut port, "0.0.0.0").unwrap();
        assert_eq!(host, "0.0.0.0");
        assert_eq!(port, 9000);

        Config::apply_addr_str(&mut host, &mut port, "192.168.1.100:8080").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 8080);

        Config::apply_addr_str(&mut host, &mut port, "localhost:5000").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5000);

        Config::apply_addr_str(&mut host, &mut port, "::1").unwrap();
        assert_eq!(host, "[::1]");

        Config::apply_addr_str(&mut host, &mut port, "[::1]:7000").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 7000);
    }

    #[test]
    fn test_parse_cli_actions_and_tls_flags() {
        let (act, cfg) = Config::parse_cli_with_base(default_test_cfg(), &[]).unwrap();
        assert_eq!(act, CliAction::Run);
        assert_eq!(cfg.port, 8097);
        assert!(cfg.tls_enabled);

        let args = vec!["-p".to_string(), "9090".to_string(), "--no-tls".to_string()];
        let (act, cfg) = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap();
        assert_eq!(act, CliAction::Run);
        assert_eq!(cfg.port, 9090);
        assert!(!cfg.tls_enabled);

        let args = vec![
            "run".to_string(),
            "--bind".to_string(),
            "0.0.0.0:8888".to_string(),
            "--tls-cert".to_string(),
            "C:\\certs\\c.pem".to_string(),
            "--tls-key=C:\\certs\\k.pem".to_string(),
        ];
        let (act, cfg) = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap();
        assert_eq!(act, CliAction::Run);
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8888);
        assert_eq!(cfg.tls_cert, Some(PathBuf::from("C:\\certs\\c.pem")));
        assert_eq!(cfg.tls_key, Some(PathBuf::from("C:\\certs\\k.pem")));

        let args = vec!["--service".to_string(), "--no-tls".to_string()];
        let (act, cfg) = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap();
        assert_eq!(act, CliAction::ServiceWorker);
        assert!(!cfg.tls_enabled);

        let args = vec!["--help".to_string()];
        let (act, _) = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap();
        assert_eq!(act, CliAction::Help);

        let args = vec!["unknown_cmd".to_string()];
        let err = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap_err();
        assert!(err.contains("Nieznane polecenie"));

        let args = vec![
            "--no-tls".to_string(),
            "--tls-cert".to_string(),
            "cert.pem".to_string(),
        ];
        let err = Config::parse_cli_with_base(default_test_cfg(), &args).unwrap_err();
        assert!(err.contains("--no-tls"));
    }

    #[test]
    fn test_resolve_tls_paths_fail_closed() {
        let mut cfg = default_test_cfg();
        cfg.tls_cert = Some(PathBuf::from("C:\\nonexistent_path_12345\\cert.pem"));
        cfg.tls_key = Some(PathBuf::from("C:\\nonexistent_path_12345\\key.pem"));
        let err = cfg.resolve_tls_paths().unwrap_err();
        assert!(err.contains("Nie znaleziono pliku certyfikatu TLS"));
    }
}
