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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use odbc_api::{
    buffers::TextRowSet, ColumnDescription, ConnectionOptions, Cursor, DataType, Environment,
    IntoParameter, Nullability, ResultSetMetadata,
};
use serde::Serialize;

use crate::encoding::{decode_bytes, to_hex};

static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static CURSOR_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Kod typu zgodny ze stałymi java.sql.Types (JDBC)
pub fn map_odbc_type(dt: DataType) -> (i32, &'static str, usize, i16) {
    match dt {
        DataType::Char { length } => (1, "CHAR", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::WChar { length } => (-15, "NCHAR", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::Numeric { precision, scale } => (2, "NUMERIC", precision, scale),
        DataType::Decimal { precision, scale } => (3, "DECIMAL", precision, scale),
        DataType::Integer => (4, "INTEGER", 10, 0),
        DataType::SmallInt => (5, "SMALLINT", 5, 0),
        DataType::Float { precision } => (6, "FLOAT", precision, 0),
        DataType::Real => (7, "REAL", 7, 0),
        DataType::Double => (8, "DOUBLE", 15, 0),
        DataType::Varchar { length } => (12, "VARCHAR", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::WVarchar { length } => (-9, "NVARCHAR", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::LongVarchar { length } => (-1, "LONGVARCHAR", length.map(|l| l.get()).unwrap_or(65535), 0),
        DataType::WLongVarchar { length } => (-16, "LONGNVARCHAR", length.map(|l| l.get()).unwrap_or(65535), 0),
        DataType::Date => (91, "DATE", 10, 0),
        DataType::Time { precision } => (92, "TIME", 8, precision),
        DataType::Timestamp { precision } => (93, "TIMESTAMP", 26, precision),
        DataType::BigInt => (-5, "BIGINT", 19, 0),
        DataType::TinyInt => (-6, "TINYINT", 3, 0),
        DataType::Bit => (-7, "BIT", 1, 0),
        DataType::Binary { length } => (-2, "BINARY", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::Varbinary { length } => (-3, "VARBINARY", length.map(|l| l.get()).unwrap_or(255), 0),
        DataType::LongVarbinary { length } => (-4, "LONGVARBINARY", length.map(|l| l.get()).unwrap_or(65535), 0),
        DataType::Other { data_type, column_size, decimal_digits } => {
            let size = column_size.map(|l| l.get()).unwrap_or(255);
            if data_type.0 == -10 {
                (-16, "LONGNVARCHAR", size, decimal_digits)
            } else {
                (12, "VARCHAR", size, decimal_digits)
            }
        }
        DataType::Unknown => (12, "VARCHAR", 255, 0),
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ColumnMeta {
    pub name: String,
    pub sql_type: i32,
    pub type_name: String,
    pub precision: usize,
    pub scale: i16,
    pub nullable: i32,
}

#[derive(Serialize, Debug)]
pub struct QueryResponse {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_id: Option<String>,
    pub has_more: bool,
}

#[derive(Serialize, Debug)]
pub struct FetchResponse {
    pub rows: Vec<Vec<Option<String>>>,
    pub has_more: bool,
}

#[derive(Serialize, Debug)]
pub struct ExecuteResponse {
    pub affected_rows: usize,
}

struct ActiveCursor {
    rows: Vec<Vec<Option<String>>>,
    offset: usize,
}

pub enum SessionCommand {
    Query {
        sql: String,
        params: Vec<Option<String>>,
        fetch_size: Option<usize>,
        charset: Option<String>,
        resp: tokio::sync::oneshot::Sender<Result<QueryResponse, String>>,
    },
    FetchMore {
        cursor_id: String,
        fetch_size: usize,
        resp: tokio::sync::oneshot::Sender<Result<FetchResponse, String>>,
    },
    CloseCursor {
        cursor_id: String,
        resp: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Execute {
        sql: String,
        params: Vec<Option<String>>,
        resp: tokio::sync::oneshot::Sender<Result<ExecuteResponse, String>>,
    },
    SetAutoCommit {
        auto_commit: bool,
        resp: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Commit {
        resp: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Rollback {
        resp: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Ping {
        resp: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    Close {
        resp: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

pub struct SessionHandle {
    pub sender: Sender<SessionCommand>,
    pub last_active: Instant,
}

pub struct SessionManager {
    env: Environment,
    sessions: tokio::sync::RwLock<HashMap<String, SessionHandle>>,
}

impl SessionManager {
    pub fn new(env: Environment) -> Self {
        Self {
            env,
            sessions: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Bezpiecznie formatuje parametr connection stringa
    pub fn format_conn_param(key: &str, val: &str) -> String {
        if val.contains(';') || val.contains('{') || val.contains('}') || val.contains(' ') {
            let escaped = val.replace('}', "}}");
            format!("{}={{{}}}", key, escaped)
        } else {
            format!("{}={}", key, val)
        }
    }

    pub fn build_conn_string(dsn: &str, user: Option<&str>, password: Option<&str>) -> String {
        let mut parts = vec![Self::format_conn_param("DSN", dsn)];
        if let Some(u) = user {
            if !u.is_empty() {
                parts.push(Self::format_conn_param("UID", u));
            }
        }
        if let Some(p) = password {
            if !p.is_empty() {
                parts.push(Self::format_conn_param("PWD", p));
            }
        }
        parts.join(";")
    }

    /// Tworzy nową sesję i uruchamia jej dedykowany wątek OS
    pub async fn create_session(
        &self,
        dsn: &str,
        user: Option<&str>,
        password: Option<&str>,
        charset: Option<String>,
    ) -> Result<String, String> {
        let conn_str = Self::build_conn_string(dsn, user, password);
        let id_num = SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let session_id = format!("sess_{}_{}", now_ts, id_num);

        let (cmd_tx, cmd_rx) = channel::<SessionCommand>();
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        // Bezpieczne sklonowanie wskaźnika do środowiska dla wątku sesji
        // Environment jest bezpieczny do użycia w wielu wątkach
        let env_ptr = &self.env as *const Environment as usize;
        let default_charset = charset.clone();
        let _thread_session_id = session_id.clone();

        std::thread::Builder::new()
            .name(format!("odbc-sess-{}", id_num))
            .spawn(move || {
                let env = unsafe { &*(env_ptr as *const Environment) };
                let connect_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    env.connect_with_connection_string(&conn_str, ConnectionOptions::default())
                }));

                let mut conn = match connect_res {
                    Ok(Ok(c)) => {
                        let _ = init_tx.send(Ok(()));
                        c
                    }
                    Ok(Err(e)) => {
                        let _ = init_tx.send(Err(format!("Błąd połączenia ODBC: {}", e)));
                        return;
                    }
                    Err(panic_err) => {
                        let msg = panic_to_string(panic_err);
                        let _ = init_tx.send(Err(format!("Panic podczas łączenia z ODBC: {}", msg)));
                        return;
                    }
                };

                let mut active_cursors: HashMap<String, ActiveCursor> = HashMap::new();

                // Pętla obsługi komend na DEDYKOWANYM wątku sesji
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        SessionCommand::Query {
                            sql,
                            params,
                            fetch_size,
                            charset,
                            resp,
                        } => {
                            let effective_charset = charset.or_else(|| default_charset.clone());
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                execute_query_worker(
                                    &mut conn,
                                    &sql,
                                    &params,
                                    fetch_size,
                                    effective_charset.as_deref(),
                                    &mut active_cursors,
                                )
                            }));

                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas wykonywania zapytania: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::FetchMore {
                            cursor_id,
                            fetch_size,
                            resp,
                        } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                fetch_more_worker(&cursor_id, fetch_size, &mut active_cursors)
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas pobierania paczki wierszy: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::CloseCursor { cursor_id, resp } => {
                            active_cursors.remove(&cursor_id);
                            let _ = resp.send(Ok(()));
                        }
                        SessionCommand::Execute {
                            sql,
                            params,
                            resp,
                        } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                execute_statement_worker(&mut conn, &sql, &params)
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas wykonywania instrukcji execute: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::SetAutoCommit { auto_commit, resp } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                conn.set_autocommit(auto_commit).map_err(|e| e.to_string())
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas set_autocommit: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::Commit { resp } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                conn.commit().map_err(|e| e.to_string())
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas commit: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::Rollback { resp } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                conn.rollback().map_err(|e| e.to_string())
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas rollback: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::Ping { resp } => {
                            let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                match conn.is_dead() {
                                    Ok(dead) => Ok(!dead),
                                    Err(_) => Ok(true), // Jeśli sterownik nie wspiera is_dead, traktujemy jako żywe
                                }
                            }));
                            let final_res = match run_res {
                                Ok(res) => res,
                                Err(p) => Err(format!("Panic podczas ping: {}", panic_to_string(p))),
                            };
                            let _ = resp.send(final_res);
                        }
                        SessionCommand::Close { resp } => {
                            active_cursors.clear();
                            let _ = resp.send(Ok(()));
                            break; // Kończymy pętlę i wątek
                        }
                    }
                }

                // Wątek sesji thread_session_id zakończył pracę.
            })
            .map_err(|e| format!("Nie udało się uruchomić wątku sesji: {}", e))?;

        // Czekamy na wynik nawiązania połączenia przez wątek roboczy
        init_rx
            .await
            .map_err(|_| "Wątek sesji zakończył się nieoczekiwanie podczas inicjalizacji".to_string())??;

        let handle = SessionHandle {
            sender: cmd_tx,
            last_active: Instant::now(),
        };

        self.sessions
            .write()
            .await
            .insert(session_id.clone(), handle);

        Ok(session_id)
    }

    /// Pobiera kopię nadajnika dla danej sesji i odświeża znacznik aktywności
    pub async fn get_sender(&self, session_id: &str) -> Option<Sender<SessionCommand>> {
        let mut map = self.sessions.write().await;
        if let Some(h) = map.get_mut(session_id) {
            h.last_active = Instant::now();
            Some(h.sender.clone())
        } else {
            None
        }
    }

    /// Zamyka i usuwa sesję
    pub async fn close_session(&self, session_id: &str) -> Result<(), String> {
        let handle = self.sessions.write().await.remove(session_id);
        if let Some(h) = handle {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = h.sender.send(SessionCommand::Close { resp: tx });
            let _ = rx.await;
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Zamyka wszystkie aktywne sesje (wywoływane przy zatrzymywaniu serwera)
    pub async fn close_all_sessions(&self) {
        let ids: Vec<String> = {
            let map = self.sessions.read().await;
            map.keys().cloned().collect()
        };
        for id in ids {
            println!("Zamykanie sesji {} podczas wyłączania serwera...", id);
            let _ = self.close_session(&id).await;
        }
    }

    /// Sprząta nieaktywne sesje (idle timeout, np. 30 minut)
    pub async fn cleanup_idle_sessions(&self, max_idle: Duration) {
        let mut to_close = Vec::new();
        {
            let map = self.sessions.read().await;
            for (id, h) in map.iter() {
                if h.last_active.elapsed() > max_idle {
                    to_close.push(id.clone());
                }
            }
        }

        for id in to_close {
            println!("Zamykanie bezczynnej sesji: {}", id);
            let _ = self.close_session(&id).await;
        }
    }

    /// Wykonuje jednorazowe zapytanie bez tworzenia trwałej sesji (dla kompatybilności wstecznej)
    pub async fn execute_ephemeral_query(
        &self,
        dsn: &str,
        user: Option<&str>,
        password: Option<&str>,
        sql: String,
        params: Vec<Option<String>>,
        fetch_size: Option<usize>,
        charset: Option<String>,
    ) -> Result<QueryResponse, String> {
        let sess_id = self.create_session(dsn, user, password, charset.clone()).await?;
        let sender = self.get_sender(&sess_id).await.ok_or("Sesja zniknęła")?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .send(SessionCommand::Query {
                sql,
                params,
                fetch_size,
                charset,
                resp: tx,
            })
            .map_err(|e| e.to_string())?;

        let result = rx.await.map_err(|_| "Brak odpowiedzi od wątku sesji")?;
        let _ = self.close_session(&sess_id).await;
        result
    }

    /// Wykonuje jednorazową instrukcję DML/DDL bez trwałej sesji
    pub async fn execute_ephemeral_execute(
        &self,
        dsn: &str,
        user: Option<&str>,
        password: Option<&str>,
        sql: String,
        params: Vec<Option<String>>,
        charset: Option<String>,
    ) -> Result<ExecuteResponse, String> {
        let sess_id = self.create_session(dsn, user, password, charset.clone()).await?;
        let sender = self.get_sender(&sess_id).await.ok_or("Sesja zniknęła")?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .send(SessionCommand::Execute {
                sql,
                params,
                resp: tx,
            })
            .map_err(|e| e.to_string())?;

        let result = rx.await.map_err(|_| "Brak odpowiedzi od wątku sesji")?;
        let _ = self.close_session(&sess_id).await;
        result
    }
}

fn panic_to_string(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "Nieznany błąd (panic)".to_string()
    }
}

fn execute_query_worker(
    conn: &mut odbc_api::Connection<'_>,
    sql: &str,
    params: &[Option<String>],
    fetch_size: Option<usize>,
    charset_hint: Option<&str>,
    active_cursors: &mut HashMap<String, ActiveCursor>,
) -> Result<QueryResponse, String> {
    let mut columns = Vec::new();
    let mut all_rows = Vec::new();

    // Wykonanie zapytania z obsługą parametrów (w tym wartości NULL)
    let maybe_cursor = if params.is_empty() {
        conn.execute(sql, (), None)
            .map_err(|e| format!("Błąd wykonania zapytania: {}", e))?
    } else {
        let bindable: Vec<_> = params
            .iter()
            .map(|opt| opt.as_deref().into_parameter())
            .collect();
        conn.execute(sql, bindable.as_slice(), None)
            .map_err(|e| format!("Błąd wykonania zapytania z parametrami: {}", e))?
    };

    if let Some(mut cursor) = maybe_cursor {
        let num_cols = cursor.num_result_cols().map_err(|e| e.to_string())?;
        let mut col_desc = ColumnDescription::default();
        let mut binary_flags = Vec::with_capacity(num_cols as usize);

        for i in 1..=num_cols {
            let col_idx = i as u16;
            cursor.describe_col(col_idx, &mut col_desc).map_err(|e| e.to_string())?;

            let name = col_desc
                .name_to_string()
                .unwrap_or_else(|_| format!("COL_{}", col_idx));

            let (sql_type, type_name, precision, scale) = map_odbc_type(col_desc.data_type);
            let nullable = match col_desc.nullability {
                Nullability::NoNulls => 0,
                Nullability::Nullable => 1,
                Nullability::Unknown => 2,
            };

            let is_binary = matches!(sql_type, -2 | -3 | -4);
            binary_flags.push(is_binary);

            columns.push(ColumnMeta {
                name,
                sql_type,
                type_name: type_name.to_string(),
                precision,
                scale,
                nullable,
            });
        }

        // Buforujemy wiersze: bezpieczny rozmiar komórki tekstu 65536 B (64 KB)
        let mut buffers = TextRowSet::for_cursor(250, &mut cursor, Some(65536))
            .map_err(|e| format!("Błąd alokacji bufora TextRowSet: {}", e))?;
        let mut row_set_cursor = cursor
            .bind_buffer(&mut buffers)
            .map_err(|e| format!("Błąd powiązania bufora: {}", e))?;

        while let Some(batch) = row_set_cursor.fetch().map_err(|e| format!("Błąd fetch: {}", e))? {
            for row_index in 0..batch.num_rows() {
                let mut row_vec = Vec::with_capacity(num_cols as usize);
                for col_index in 0..num_cols as usize {
                    let cell_bytes = batch.at(col_index, row_index);
                    let cell_str = match cell_bytes {
                        Some(bytes) => {
                            if binary_flags[col_index] {
                                Some(to_hex(bytes))
                            } else {
                                Some(decode_bytes(bytes, charset_hint))
                            }
                        }
                        None => None,
                    };
                    row_vec.push(cell_str);
                }
                all_rows.push(row_vec);
            }
        }
    }
    // Kursory i statement zostały zamknięte po wyjściu z bloku, połączenie ODBC jest WOLNE!

    // Obsługa stronicowania (fetch_size)
    let limit = fetch_size.unwrap_or(0);
    if limit > 0 && all_rows.len() > limit {
        let cursor_id = format!("cur_{}", CURSOR_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        let first_chunk = all_rows.drain(0..limit).collect();

        active_cursors.insert(
            cursor_id.clone(),
            ActiveCursor {
                rows: all_rows,
                offset: 0,
            },
        );

        Ok(QueryResponse {
            columns,
            rows: first_chunk,
            cursor_id: Some(cursor_id),
            has_more: true,
        })
    } else {
        Ok(QueryResponse {
            columns,
            rows: all_rows,
            cursor_id: None,
            has_more: false,
        })
    }
}

fn fetch_more_worker(
    cursor_id: &str,
    fetch_size: usize,
    active_cursors: &mut HashMap<String, ActiveCursor>,
) -> Result<FetchResponse, String> {
    let cur = active_cursors
        .get_mut(cursor_id)
        .ok_or_else(|| format!("Nie znaleziono kursora: {}", cursor_id))?;

    let remaining = cur.rows.len() - cur.offset;
    let take_count = std::cmp::min(remaining, fetch_size);

    let rows: Vec<_> = cur.rows[cur.offset..cur.offset + take_count]
        .iter()
        .cloned()
        .collect();

    cur.offset += take_count;
    let has_more = cur.offset < cur.rows.len();

    if !has_more {
        active_cursors.remove(cursor_id);
    }

    Ok(FetchResponse { rows, has_more })
}

fn execute_statement_worker(
    conn: &mut odbc_api::Connection<'_>,
    sql: &str,
    params: &[Option<String>],
) -> Result<ExecuteResponse, String> {
    let mut stmt = conn
        .preallocate()
        .map_err(|e| format!("Błąd preallocate statement: {}", e))?;

    if params.is_empty() {
        stmt.execute(sql, ())
            .map_err(|e| format!("Błąd execute: {}", e))?;
    } else {
        let bindable: Vec<_> = params
            .iter()
            .map(|opt| opt.as_deref().into_parameter())
            .collect();
        stmt.execute(sql, bindable.as_slice())
            .map_err(|e| format!("Błąd execute z parametrami: {}", e))?;
    }

    let count = stmt.row_count().map_err(|e| e.to_string())?.unwrap_or(0);
    Ok(ExecuteResponse {
        affected_rows: count,
    })
}
