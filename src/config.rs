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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub const DEFAULT_HOST: &'static str = "127.0.0.1";
    pub const DEFAULT_PORT: u16 = 8097;

    pub fn from_env_and_args() -> Result<Option<Self>, String> {
        let mut host = Self::DEFAULT_HOST.to_string();
        let mut port = Self::DEFAULT_PORT;

        // 1. Zmienne środowiskowe (ODBC_PROXY_ADDR / BIND_ADDR, ODBC_PROXY_PORT / PORT)
        if let Ok(addr) = std::env::var("ODBC_PROXY_ADDR").or_else(|_| std::env::var("BIND_ADDR")) {
            Self::apply_addr_str(&mut host, &mut port, &addr)?;
        }
        if let Ok(port_str) = std::env::var("ODBC_PROXY_PORT").or_else(|_| std::env::var("PORT")) {
            port = port_str
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("Nieprawidłowy port w zmiennej środowiskowej: '{}'", port_str))?;
        }

        // 2. Argumenty wiersza poleceń (CLI ma pierwszeństwo przed zmiennymi środowiskowymi)
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::parse_args_into(&mut host, &mut port, &args)
    }

    pub fn parse_args_into(
        host: &mut String,
        port: &mut u16,
        args: &[String],
    ) -> Result<Option<Self>, String> {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "-h" || arg == "--help" {
                Self::print_help();
                return Ok(None);
            } else if arg == "-b" || arg == "--bind" || arg == "-a" || arg == "--addr" {
                i += 1;
                if i >= args.len() {
                    return Err(format!("Brak wartości po fladze '{}'", arg));
                }
                Self::apply_addr_str(host, port, &args[i])?;
            } else if let Some(val) = arg
                .strip_prefix("--bind=")
                .or_else(|| arg.strip_prefix("--addr="))
                .or_else(|| arg.strip_prefix("-b="))
                .or_else(|| arg.strip_prefix("-a="))
            {
                Self::apply_addr_str(host, port, val)?;
            } else if arg == "-p" || arg == "--port" {
                i += 1;
                if i >= args.len() {
                    return Err(format!("Brak wartości po fladze '{}'", arg));
                }
                *port = args[i]
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| format!("Nieprawidłowy numer portu: '{}'", args[i]))?;
            } else if let Some(val) = arg.strip_prefix("--port=").or_else(|| arg.strip_prefix("-p=")) {
                *port = val
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| format!("Nieprawidłowy numer portu: '{}'", val))?;
            } else {
                return Err(format!(
                    "Nieznany argument: '{}'. Użyj --help, aby wyświetlić dostępne opcje.",
                    arg
                ));
            }
            i += 1;
        }

        Ok(Some(Self {
            host: host.clone(),
            port: *port,
        }))
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

    pub fn print_help() {
        println!("Rust ODBC Proxy");
        println!();
        println!("SPOSÓB UŻYCIA:");
        println!("    odbc-rust-proxy [OPCJE]");
        println!();
        println!("OPCJE:");
        println!("    -b, --bind, -a, --addr <ADRES>    Adres do nasłuchu (np. 127.0.0.1:8097, 0.0.0.0, 8097)");
        println!("                                      [domyślnie: 127.0.0.1:8097]");
        println!("    -p, --port <PORT>                 Port do nasłuchu (np. 8097)");
        println!("    -h, --help                        Wyświetla pomoc i kończy działanie");
        println!();
        println!("ZMIENNE ŚRODOWISKOWE:");
        println!("    ODBC_PROXY_ADDR, BIND_ADDR        Adres do nasłuchu (host:port lub host)");
        println!("    ODBC_PROXY_PORT, PORT             Port do nasłuchu");
        println!();
        println!("PRIORYTET:");
        println!("    Argumenty CLI > Zmienne środowiskowe > Wartości domyślne (127.0.0.1:8097)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_addr_str_cases() {
        let mut host = "127.0.0.1".to_string();
        let mut port = 8097;

        // Sam port jako string
        Config::apply_addr_str(&mut host, &mut port, "9000").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 9000);

        // Sam adres IP
        Config::apply_addr_str(&mut host, &mut port, "0.0.0.0").unwrap();
        assert_eq!(host, "0.0.0.0");
        assert_eq!(port, 9000);

        // Pełny adres SocketAddr IPv4
        Config::apply_addr_str(&mut host, &mut port, "192.168.1.100:8080").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 8080);

        // Hostname z portem
        Config::apply_addr_str(&mut host, &mut port, "localhost:5000").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5000);

        // Sam hostname
        Config::apply_addr_str(&mut host, &mut port, "example.local").unwrap();
        assert_eq!(host, "example.local");
        assert_eq!(port, 5000);

        // IPv6
        Config::apply_addr_str(&mut host, &mut port, "::1").unwrap();
        assert_eq!(host, "[::1]");

        Config::apply_addr_str(&mut host, &mut port, "[::1]:7000").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 7000);
    }

    #[test]
    fn test_parse_args() {
        let mut host = Config::DEFAULT_HOST.to_string();
        let mut port = Config::DEFAULT_PORT;

        let args = vec!["-p".to_string(), "9090".to_string()];
        let cfg = Config::parse_args_into(&mut host, &mut port, &args).unwrap().unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 9090);

        let args = vec!["--bind".to_string(), "0.0.0.0:8888".to_string()];
        let cfg = Config::parse_args_into(&mut host, &mut port, &args).unwrap().unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8888);

        let args = vec!["--port=7777".to_string()];
        let cfg = Config::parse_args_into(&mut host, &mut port, &args).unwrap().unwrap();
        assert_eq!(cfg.port, 7777);

        let args = vec!["--help".to_string()];
        let res = Config::parse_args_into(&mut host, &mut port, &args).unwrap();
        assert!(res.is_none());

        let args = vec!["--unknown".to_string()];
        let err = Config::parse_args_into(&mut host, &mut port, &args).unwrap_err();
        assert!(err.contains("Nieznany argument"));
    }
}
