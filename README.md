# ODBC Rust Proxy

Wysokowydajny mikroserwis proxy napisany w języku **Rust**, pośredniczący pomiędzy nowoczesnymi aplikacjami 64-bitowymi (np. Java 17/21+) a **32-bitowymi sterownikami ODBC pod systemem Windows** (np. Microsoft Access / Jet, Visual FoxPro, Btrieve / Pervasive, dBase, starsze bazy systemów ERP/FK).

Projekt stanowi bezpośredni zamiennik wycofanego w nowszych wersjach Javy mostka `sun.jdbc.odbc.JdbcOdbcDriver`, zapewniając pełną transakcyjność, izolację wątkową (Thread Affinity), obsługę polskich stron kodowych (w tym DOS CP852) oraz strumieniowanie i stronicowanie danych.

---

## Spis treści
1. [Kluczowe cechy](#kluczowe-cechy)
2. [Architektura i zasady działania](#architektura-i-zasady-działania)
3. [Zarządzanie sesjami i wątkami](#zarządzanie-sesjami-i-wątkami)
4. [Obsługa awarii klienta (Session Reaper)](#obsługa-awarii-klienta-session-reaper)
5. [Wymagania i kompilacja](#wymagania-i-kompilacja)
6. [Konfiguracja](#konfiguracja)
7. [Strony kodowe i dane binarne](#strony-kodowe-i-dane-binarne)
8. [Mapowanie typów danych](#mapowanie-typów-danych)
9. [Specyfikacja API (Endpointy HTTP)](#specyfikacja-api-endpointy-http)
   - [POST /session/connect](#post-sessionconnect)
   - [POST /session/autocommit](#post-sessionautocommit)
   - [POST /session/commit](#post-sessioncommit)
   - [POST /session/rollback](#post-sessionrollback)
   - [POST /session/ping](#post-sessionping)
   - [POST /session/close](#post-sessionclose)
   - [POST /session/fetch](#post-sessionfetch)
   - [POST /session/close-cursor](#post-sessionclose-cursor)
   - [POST /query](#post-query)
   - [POST /execute](#post-execute)
10. [Wdrażanie jako usługa Windows](#wdrażanie-jako-usługa-windows)
11. [Rozwiązywanie problemów (Troubleshooting)](#rozwiązywanie-problemów-troubleshooting)

---

## Kluczowe cechy

- **Mostek 64-bit do 32-bit**: Aplikacja 64-bitowa (np. Java 21) komunikuje się po lokalnym HTTP z 32-bitowym procesem proxy, który posiada natywny dostęp do podsystemu 32-bitowego ODBC (`SysWOW64\odbcad32.exe`).
- **Thread Affinity (STA)**: Każda otwarta sesja bazy danych posiada **dedykowany wątek systemu operacyjnego Windows**, co eliminuje błędy pamięci i wyścigi w starszych driverach ODBC nieprzystosowanych do wielowątkowości.
- **Pełna transakcyjność**: Obsługa wyłączenia auto-commita (`auto_commit: false`), jawnego zatwierdzania (`commit`) i wycofywania zmian (`rollback`) w ramach tego samego fizycznego połączenia.
- **Brak blokowania połączenia przez otwarte kursory**: Serwer buforuje wiersze zapytania w pamięci sesji i natychmiast zwalnia natywny kursor ODBC, co eliminuje błąd `[Microsoft][ODBC Driver Manager] Connection is busy with results for another hstmt` przy wykonywaniu operacji po wierszu (`UPDATE`/`INSERT`).
- **Paginacja wyników (`fetch_size`)**: Możliwość pobierania wyników w paczkach (np. po 500 wierszy), chroniąca przed problemami z pamięcią (OOM) przy dużych zbiorach danych.
- **Poprawna obsługa wartości `NULL`**: Parametry `null` są bindowane natywnie jako `SQL_NULL_DATA`.
- **Pełna obsługa polskich znaków**: Wbudowane dekodowanie dla **CP852** (DOS Latin-2 / EE8PC852), **Windows-1250**, **ISO-8859-2** oraz **UTF-8**.
- **Ochrona danych binarnych**: Kolumny `BINARY`, `VARBINARY`, `LONGVARBINARY` są automatycznie zwracane jako ciągi heksadecymalne (HEX), chroniąc przed uszkodzeniem bajtów.
- **Bezpieczeństwo**: Automatyczne zabezpieczenie parametrów connection stringa klamrami `{...}` przeciwko atakom typu *Connection String Injection*.
- **Odporność na awarie**: obsługa `std::panic::catch_unwind` – błąd w jednym połączeniu nie ubija procesu proxy.

---

## Architektura i zasady działania

Aplikacja łączy asynchroniczny serwer HTTP oparty o framework **Axum** i runtime **Tokio** ze synchronicznym, dedykowanym modelem wątków dla podsystemu ODBC.

```
+-------------------------------------------------------------------------+
|                              Klient HTTP                                |
|           (np. RustOdbcDriver w Java 21 / aplikacja efkawww)            |
+-------------------------------------------------------------------------+
                                    |
                         Lokalne HTTP (JSON)
                                    v
+-------------------------------------------------------------------------+
|                  Tokio Async Runtime (Axum Router)                      |
|                                                                         |
|  - Asynchroniczna obsługa zapytań HTTP na porcie 8097                   |
|  - SessionManager z bezpieczną mapą sesji:                              |
|    RwLock<HashMap<String, SessionHandle>>                               |
|  - Zadanie w tle: Session Reaper (sprawdzanie timeoutu bezczynności)    |
+-------------------------------------------------------------------------+
           |                                             |
     Kanał mpsc                                    Kanał mpsc
           v                                             v
+-----------------------------+               +-----------------------------+
|    Wątek OS 1 (Sesja A)     |               |    Wątek OS 2 (Sesja B)     |
|  - loop na cmd_rx.recv()    |               |  - loop na cmd_rx.recv()    |
|  - Trwały uchwyt ODBC       |               |  - Trwały uchwyt ODBC       |
|  - Pamięć kursorów sesji    |               |  - Pamięć kursorów sesji    |
+-----------------------------+               +-----------------------------+
           |                                             |
  Natywne wywołania C                           Natywne wywołania C
           v                                             v
+-----------------------------+               +-----------------------------+
|   32-bit ODBC Driver DLL    |               |   32-bit ODBC Driver DLL    |
|   (np. Access, FoxPro, FK)  |               |   (np. Access, FoxPro, FK)  |
+-----------------------------+               +-----------------------------+
```

---

## Zarządzanie sesjami i wątkami

1. **Inicjalizacja sesji**:
   - Klient wysyła żądanie `POST /session/connect`.
   - Serwer uruchamia nowy wątek OS (`std::thread::Builder::new().name("odbc-sess-...").spawn(...)`).
   - Wątek łączy się ze źródłem ODBC za pomocą `env.connect_with_connection_string(...)`.
   - Wątek zgłasza sukces przez kanał `oneshot` i przechodzi w stan oczekiwania na komendy w pętli `cmd_rx.recv()`.
   - Klient otrzymuje unikalny identyfikator `session_id` (np. `sess_1731180000_1`).
2. **Wykonywanie operacji**:
   - Wszystkie kolejne operacje (`/query`, `/execute`, `/session/autocommit`, `/session/commit`, `/session/rollback`, `/session/fetch`) przekazują otrzymany `session_id`.
   - Router odnajduje kanał do wątku i przekazuje zadanie.
   - Wątek wykonuje operację ODBC na tym samym fizycznym uchwycie `HDBC` i zwraca wynik przez kanał `oneshot`.
   - Podczas spoczynku wątek nie zużywa cykli procesora (**0% CPU**).
3. **Zakończenie sesji**:
   - Wywołanie `POST /session/close` zwalnia kursory, wykonuje rozłączenie `SQLDisconnect` i kończy wątek OS.

---

## Obsługa awarii klienta (Session Reaper)

Jeśli aplikacja klienta ulegnie awarii (np. `kill -9`, awaria JVM, błąd `OutOfMemoryError`, wyłączenie komputera) i **nie wywoła endpointu `/session/close`**:

1. W serwerze proxy działa w tle zadanie **Session Reaper**, budzące się co 60 sekund.
2. Reaper sprawdza czas od ostatniej aktywności (`last_active`) dla każdej sesji.
3. Jeśli czas bezczynności przekroczy zdefiniowany limit (domyślnie **30 minut** / 1800 sekund):
   - Sesja zostaje usunięta z rejestru.
   - Kanał wątku zostaje zamknięty lub wysyłana jest komenda zamknięcia.
   - Wątek sesji budzi się i uruchamia procedurę zwalniania zasobów (`Drop` dla `Connection` z biblioteki `odbc-api`).
   - Jeśli w ramach sesji otwarta była niezatwierdzona transakcja, biblioteka ODBC automatycznie wykonuje **`SQL_ROLLBACK`** na bazie danych, zwalniając blokady tabel i wierszy.
   - Następuje wywołanie natywnego **`SQLDisconnect`** i zwolnienie uchwytów ODBC.
   - Wątek kończy bieg, a system Windows zwalnia stos pamięci (~1 MB).

---

## Wymagania i kompilacja

### Wymagania systemowe
- System operacyjny: **Windows 7 / 10 / 11 / Server 2012+**
- Zainstalowane środowisko **Rust** (narzędzie `rustup`)
- Microsoft Visual C++ Build Tools (MSVC)

### Kompilacja dla architektury 32-bitowej (ZALECANA DLA 32-bit ODBC)
> [!IMPORTANT]
> Aby mikroserwis widział 32-bitowe źródła danych DSN skonfigurowane w systemowym narzędziu `C:\Windows\SysWOW64\odbcad32.exe`, **musi zostać skompilowany jako proces 32-bitowy (`i686`)**.

1. Dodaj cel kompilacji 32-bit x86:
   ```cmd
   rustup target add i686-pc-windows-msvc
   ```
2. Skompiluj wersję zoptymalizowaną (Release):
   ```cmd
   cargo build --release --target i686-pc-windows-msvc
   ```
3. Wynikowy plik wykonywalny znajdzie się w:
   `target\i686-pc-windows-msvc\release\odbc-rust-proxy.exe`

### Kompilacja 64-bitowa (dla 64-bitowych sterowników ODBC)
```cmd
cargo build --release
```
Plik wykonywalny: `target\release\odbc-rust-proxy.exe`

---

## Tryby uruchomienia (Dual-Mode: Standalone / Natywna usługa Windows)

Aplikacja posiada wbudowaną, bezpośrednią integrację z **Windows Service Control Manager (SCM)** za pośrednictwem Win32 API.
Dzięki temu plik wykonywalny `odbc-rust-proxy.exe` jest **w 100% autonomiczny** — nie wymaga żadnych zewnętrznych nakładek (np. `nssm.exe`), skryptów ani skomplikowanych poleceń systemowych `sc.exe`.

### 1. Tryb interaktywny w konsoli (Standalone)
Idealny do uruchamiania deweloperskiego, testów lub debugowania z bezpośrednim podglądem logów na ekranie:
```powershell
# Uruchomienie domyślne (Ctrl+C bezpiecznie zatrzymuje serwer i zamyka sesje ODBC):
.\odbc-rust-proxy.exe

# Lub jawnie:
.\odbc-rust-proxy.exe run
```

### 2. Zarządzanie usługą systemową Windows (Self-Management CLI)
Zarządzanie usługą realizowane jest bezpośrednio z poziomu pliku `.exe` (wymaga uruchomienia terminala PowerShell / CMD jako **Administrator**):

```powershell
# 1. Instalacja jako usługa Windows (z automatycznym startem przy starcie systemu):
.\odbc-rust-proxy.exe install

# 2. Uruchomienie usługi:
.\odbc-rust-proxy.exe start

# 3. Sprawdzenie statusu usługi (zwraca stan: Running, Stopped itp. oraz PID procesu):
.\odbc-rust-proxy.exe status

# 4. Bezpieczne zatrzymanie usługi (graceful shutdown sesji ODBC):
.\odbc-rust-proxy.exe stop

# 5. Odinstalowanie usługi z systemu:
.\odbc-rust-proxy.exe uninstall
```

> [!NOTE]
> Usługa rejestrowana jest pod nazwą systemową `PsiOdbcRustProxy` oraz nazwą wyświetlaną `PSI: ODBC Rust Proxy (32-bit Bridge)`.
> Można nią sterować również za pomocą standardowych narzędzi systemowych:
> - Przystawka graficzna: `services.msc`
> - Polecenia konsoli: `net start OdbcRustProxy` / `net stop OdbcRustProxy`
> - PowerShell: `Start-Service OdbcRustProxy` / `Stop-Service OdbcRustProxy`

---

## Konfiguracja

Aplikacja jest konfigurowana za pomocą zmiennych środowiskowych:

| Zmienna środowiskowa | Domyślna wartość | Opis |
|---|---|---|
| `ODBC_PROXY_ADDR` | `127.0.0.1:8097` | Adres IP oraz port, na którym nasłuchuje serwer HTTP proxy. |
| `ODBC_IDLE_TIMEOUT_SECS` | `1800` | Czas w sekundach (domyślnie 30 minut), po którym porzucona bezczynna sesja jest automatycznie zamykana przez Reapera. |

### Przykład uruchomienia z własnymi parametrami (PowerShell):
```powershell
$env:ODBC_PROXY_ADDR = "127.0.0.1:9000"
$env:ODBC_IDLE_TIMEOUT_SECS = "600" # 10 minut
.\target\release\odbc-rust-proxy.exe
```

---

## Strony kodowe i dane binarne

W zapytaniach (`/session/connect`, `/query`, `/execute`) można przekazać parametr `charset`:

| Nazwa charsetu w żądaniu | Zastosowany dekoder | Przeznaczenie |
|---|---|---|
| `cp852`, `ibm852`, `ee8pc852`, `852` | Dedykowana pełna tablica IBM CP852 | Starsze bazy DOS, dBase, Clipper, stara eFKa (EE8PC852). |
| `cp1250`, `windows-1250`, `ee8mswin1250`, `1250` | `encoding_rs::WINDOWS_1250` | Nowsze bazy Windows (standard w polskich instalacjach Windows). |
| `iso-8859-2`, `latin2`, `ee8iso8859p2` | `encoding_rs::ISO_8859_2` | Bazy Unix/Linux kodowane w ISO Latin-2. |
| `utf-8`, `utf8` | `std::str::from_utf8` (fallback CP1250) | Uniwersalne kodowanie UTF-8. |
| *(brak parametru)* | Auto-detekcja UTF-8 z fallbackiem do Windows-1250 | Domyślna bezpieczna heurystyka. |

### Kolumny binarne
Kolumny o kodach typów JDBC `Types.BINARY` (-2), `Types.VARBINARY` (-3), `Types.LONGVARBINARY` (-4) są automatycznie konwertowane na ciągi heksadecymalne (np. bajty `[0x01, 0xFF]` zwracane są jako `"01ff"`). Klient JDBC dokonuje ich bezpośredniego odczytu do tablicy `byte[]`.

---

## Mapowanie typów danych

Natywne typy `odbc_api::DataType` są mapowane na kody standardu JDBC (`java.sql.Types`):

| Typ ODBC (`odbc-api`) | Kod `sql_type` (JDBC) | Nazwa `type_name` | Domyślna klasa Javy |
|---|---|---|---|
| `Char` | `1` | `CHAR` | `java.lang.String` |
| `WChar` | `-15` | `NCHAR` | `java.lang.String` |
| `Varchar` | `12` | `VARCHAR` | `java.lang.String` |
| `WVarchar` | `-9` | `NVARCHAR` | `java.lang.String` |
| `LongVarchar` | `-1` | `LONGVARCHAR` | `java.lang.String` |
| `WLongVarchar` | `-16` | `LONGNVARCHAR` | `java.lang.String` |
| `Numeric` | `2` | `NUMERIC` | `java.math.BigDecimal` |
| `Decimal` | `3` | `DECIMAL` | `java.math.BigDecimal` |
| `Integer` | `4` | `INTEGER` | `java.lang.Integer` |
| `SmallInt` | `5` | `SMALLINT` | `java.lang.Integer` |
| `TinyInt` | `-6` | `TINYINT` | `java.lang.Integer` |
| `BigInt` | `-5` | `BIGINT` | `java.lang.Long` |
| `Float` | `6` | `FLOAT` | `java.lang.Double` |
| `Real` | `7` | `REAL` | `java.lang.Float` |
| `Double` | `8` | `DOUBLE` | `java.lang.Double` |
| `Bit` | `-7` | `BIT` | `java.lang.Boolean` |
| `Date` | `91` | `DATE` | `java.sql.Date` |
| `Time` | `92` | `TIME` | `java.sql.Time` |
| `Timestamp` | `93` | `TIMESTAMP` | `java.sql.Timestamp` |
| `Binary` | `-2` | `BINARY` | `byte[]` |
| `Varbinary` | `-3` | `VARBINARY` | `byte[]` |
| `LongVarbinary` | `-4` | `LONGVARBINARY` | `byte[]` |

---

## Specyfikacja API (Endpointy HTTP)

Wszystkie żądania wysyłają nagłówek `Content-Type: application/json`.
W przypadku błędu serwer zwraca kod **HTTP 500** oraz strukturę:
```json
{
  "error": "Szczegółowy opis błędu z ODBC lub serwera"
}
```

---

### POST /session/connect
Otwiera nową, trwałą sesję ODBC na dedykowanym wątku roboczym.

#### Żądanie:
```json
{
  "dsn": "MojaBaza32",
  "user": "admin",
  "password": "tajne_haslo",
  "charset": "cp1250"
}
```
- `dsn` (*string, wymagane*): Nazwa źródła danych ODBC skonfigurowana w systemie Windows.
- `user` (*string, opcjonalne*): Użytkownik bazy danych.
- `password` (*string, opcjonalne*): Hasło do bazy danych.
- `charset` (*string, opcjonalne*): Domyślna strona kodowa sesji (`cp852`, `cp1250`, `iso-8859-2`, `utf-8`).

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "session_id": "sess_1731181234_1"
}
```

---

### POST /session/autocommit
Ustawia tryb transakcyjności dla podanej sesji.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1",
  "auto_commit": false
}
```
- `session_id` (*string, wymagane*): Identyfikator otwartej sesji.
- `auto_commit` (*boolean, wymagane*): `true` włącza autocommit, `false` włącza tryb transakcji ręcznej.

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "status": "ok"
}
```

---

### POST /session/commit
Zatwierdza bieżącą transakcję w ramach otwartej sesji.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1"
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "status": "ok"
}
```

---

### POST /session/rollback
Wycofuje niezamkniętą transakcję w ramach otwartej sesji.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1"
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "status": "ok"
}
```

---

### POST /session/ping
Sprawdza, czy fizyczne połączenie ODBC dla danej sesji jest nadal aktywne (`Connection.isValid`).

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1"
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "alive": true
}
```

---

### POST /session/close
Zamyka wszystkie otwarte kursory, rozłącza połączenie ODBC (`SQLDisconnect`) i kończy dedykowany wątek roboczy.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1"
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "status": "ok"
}
```

---

### POST /session/fetch
Pobiera kolejną paczkę wierszy z wcześniej zainicjowanego zapytania, które zwróciło `has_more: true`.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1",
  "cursor_id": "cur_1",
  "fetch_size": 500
}
```
- `session_id` (*string, wymagane*): Identyfikator sesji.
- `cursor_id` (*string, wymagane*): Identyfikator kursora zwrócony przez `/query`.
- `fetch_size` (*integer, opcjonalne, domyślnie 500*): Liczba wierszy do pobrania w bieżącej paczce.

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "rows": [
    ["501", "Towar 501", "12.30"],
    ["502", "Towar 502", "45.00"]
  ],
  "has_more": false
}
```
- Gdy `has_more` osiągnie `false`, kursor sesji zostaje automatycznie usunięty z pamięci serwera.

---

### POST /session/close-cursor
Przedwczesne zamknięcie kursora sesji (np. gdy aplikacja przerwie czytanie `ResultSet` przed dojściem do końca).

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1",
  "cursor_id": "cur_1"
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "status": "ok"
}
```

---

### POST /query
Wykonuje zapytanie zwracające wiersze (`SELECT`).
Może działać w trybie **sesyjnym** (zalecanym) lub **jednorazowym** (stateless).

#### Żądanie (tryb sesyjny):
```json
{
  "session_id": "sess_1731181234_1",
  "sql": "SELECT ID, NAZWA, KWOTA, DATA FROM DOKUMENTY WHERE STATUS = ? AND KOD = ?",
  "params": ["1", null],
  "fetch_size": 1000,
  "charset": "cp1250"
}
```

#### Żądanie (tryb jednorazowy - fallback):
```json
{
  "dsn": "MojaBaza32",
  "user": "admin",
  "password": "secret",
  "sql": "SELECT * FROM KRAJE",
  "params": []
}
```

- `session_id` (*string, opcjonalne*): Jeśli podany, zapytanie wykonuje się na istniejącej sesji.
- `dsn` (*string, wymagane tylko jeśli brak session_id*): Nazwa DSN dla zapytania jednorazowego.
- `sql` (*string, wymagane*): Treść zapytania SQL ze znakami `?` dla parametrów.
- `params` (*array, opcjonalne*): Tablica parametrów pozycyjnych. Może zawierać ciągi tekstowe lub `null`.
- `fetch_size` (*integer, opcjonalne*): Maksymalna liczba wierszy zwracana w pierwszej odpowiedzi. Jeśli wyników jest więcej, zwracany jest `cursor_id` i `has_more: true`.
- `charset` (*string, opcjonalne*): Nadpisanie strony kodowej dla tego zapytania.

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "columns": [
    {
      "name": "ID",
      "sql_type": 4,
      "type_name": "INTEGER",
      "precision": 10,
      "scale": 0,
      "nullable": 0
    },
    {
      "name": "NAZWA",
      "sql_type": 12,
      "type_name": "VARCHAR",
      "precision": 100,
      "scale": 0,
      "nullable": 1
    },
    {
      "name": "KWOTA",
      "sql_type": 3,
      "type_name": "DECIMAL",
      "precision": 15,
      "scale": 2,
      "nullable": 1
    }
  ],
  "rows": [
    ["1", "Faktura VAT 1/2026", "1500.00"],
    ["2", null, "200.50"]
  ],
  "cursor_id": "cur_1",
  "has_more": true
}
```

---

### POST /execute
Wykonuje instrukcję modyfikującą dane (`INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE` itp.) i zwraca liczbę zmienionych wierszy.

#### Żądanie:
```json
{
  "session_id": "sess_1731181234_1",
  "sql": "UPDATE DOKUMENTY SET ZATWIERDZONY = ? WHERE ID = ?",
  "params": ["T", "1234"]
}
```

#### Odpowiedź sukcesu (HTTP 200):
```json
{
  "affected_rows": 1
}
```

---

## Bezpieczne zatrzymywanie usługi (Graceful Shutdown)

Zarówno w trybie konsolowym (przy wciśnięciu `Ctrl+C`), jak i w trybie usługi Windows (przy wysłaniu sygnału `Stop` z poziomu `services.msc`, `net stop` lub `odbc-rust-proxy.exe stop`):

1. Windows Service Control Manager wysyła sygnał `SERVICE_CONTROL_STOP` do procedury obsługi zdarzeń.
2. Serwer HTTP Axum natychmiast przestaje przyjmować nowe połączenia przy użyciu procedury `with_graceful_shutdown`.
3. Mechanizm Session Managera iteruje po wszystkich aktywnych sesjach ODBC (`close_all_sessions`), wysyła żądanie wycofania transakcji (`ROLLBACK`), zwalnia aktywne kursory i bezpiecznie rozłącza uchwyty ODBC (`SQLDisconnect`).
4. Wątki sesyjne OS kończą pracę i zwalniają pamięć systemową.
5. Usługa zgłasza do SCM stan `SERVICE_STOPPED` z kodem wyjścia 0 i proces kończy działanie.

Dzięki temu eliminowane jest ryzyko wiszących blokad tabel, niezatwierdzonych transakcji czy uszkodzenia plików bazodanowych.

---

## Rozwiązywanie problemów (Troubleshooting)

### 1. Serwer nie widzi 32-bitowych źródeł danych ODBC
- **Objaw**: Komunikat błędu `[Microsoft][ODBC Driver Manager] Data source name not found and no default driver specified`.
- **Przyczyna**: Binarka proxy została skompilowana dla architektury 64-bitowej (`x86_64`) i widzi wyłącznie 64-bitowe źródła ODBC.
- **Rozwiązanie**: Skompiluj projekt poleceniem `cargo build --release --target i686-pc-windows-msvc`. Podczas startu serwer powinien wypisać: `Architektura procesu: 32-bit (x86)`.

### 2. Błędne wyświetlanie polskich znaków (krzaki)
- **Objaw**: Litery takie jak `ą`, `ę`, `ł`, `ś` wyświetlają się jako nieczytelne symbole.
- **Przyczyna**: Baza danych używa kodowania DOS Latin-2 (CP852 / Mazovia), a serwer domyślnie przyjął Windows-1250.
- **Rozwiązanie**: W URL JDBC lub parametrach żądania przekaż parametr kodowania: `?charset=cp852` (lub `?charSet=EE8PC852`).

### 3. Port 8097 jest już zajęty
- **Objaw**: `Os { code: 10048, kind: AddrInUse, message: "Only one usage of each socket address is normally permitted." }`.
- **Rozwiązanie**: Zmień port za pomocą zmiennej środowiskowej `ODBC_PROXY_ADDR=127.0.0.1:8099`.
