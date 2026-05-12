# Native Windows installation

Shelly Multiplexer ships a single self-contained `shelly-multiplexer.exe`
for Windows 10 / 11 (x86_64). No installer, no dependencies.

## 1. Download

Grab the latest
`shelly-multiplexer-<version>-x86_64-pc-windows-msvc.zip` from
<https://github.com/hilman2/shelly-multiplexer/releases> and unpack it
somewhere like `C:\Program Files\ShellyMultiplexer\`.

The archive contains:

- `shelly-multiplexer.exe` — the binary
- `config.example.toml` — config template
- `README.md` — this file

## 2. Prerequisites

> **IMPORTANT** — before starting, move the real Shelly Pro 3EM off the
> default UDP-RPC port 1010 (e.g. to 2020) in the Shelly web UI:
> *Settings → Advanced → Outbound RPC / UDP-RPC*. Otherwise nothing
> can bind 1010 on the same LAN.

## 3. Configure

Copy `config.example.toml` to `config.toml` next to the .exe and edit
it — at minimum set `[real_shelly] host` to your Shelly Pro 3EM IP and
add your `[[batteries]]` and `[[circuits]]` blocks.

## 4. Run interactively (first try)

Open PowerShell **as Administrator** in the install directory and run:

```powershell
.\shelly-multiplexer.exe --config .\config.toml
```

Admin rights are needed because the virtual Shelly binds UDP 1010 and
HTTP 80 — both privileged ports on Windows.

Open the admin UI at <http://localhost:8080>. Stop with Ctrl+C.

## 5. Run as a Windows service (recommended)

Use [NSSM](https://nssm.cc) or
[WinSW](https://github.com/winsw/winsw) to wrap the .exe as a service.
Quick path with NSSM:

```powershell
# Download NSSM, then:
nssm install ShellyMultiplexer "C:\Program Files\ShellyMultiplexer\shelly-multiplexer.exe"
nssm set ShellyMultiplexer AppParameters "--config C:\Program Files\ShellyMultiplexer\config.toml"
nssm set ShellyMultiplexer AppDirectory "C:\Program Files\ShellyMultiplexer"
nssm set ShellyMultiplexer Start SERVICE_AUTO_START
nssm set ShellyMultiplexer ObjectName LocalSystem
nssm start ShellyMultiplexer
```

The service runs under LocalSystem so privileged ports are no problem.

## 6. Firewall

When Windows Defender prompts for network access, allow it on **both**
the Private and Domain profiles. Required:

- UDP 1010 in/out (Marstek polls)
- TCP 80 in (Marstek HTTP probes)
- TCP 8080 in (admin UI from your LAN)
- UDP 5353 in/out (mDNS)

## 7. Upgrade

Stop the service, replace `shelly-multiplexer.exe` with the new
version, start the service again. The config is preserved.

```powershell
nssm stop ShellyMultiplexer
# overwrite the .exe with the new build
nssm start ShellyMultiplexer
```

## 8. Logs

NSSM can redirect stdout/stderr to a file:

```powershell
nssm set ShellyMultiplexer AppStdout "C:\ProgramData\ShellyMultiplexer\log.txt"
nssm set ShellyMultiplexer AppStderr "C:\ProgramData\ShellyMultiplexer\log.txt"
nssm set ShellyMultiplexer AppRotateFiles 1
nssm restart ShellyMultiplexer
```

Set the log level with the `RUST_LOG` env var:

```powershell
nssm set ShellyMultiplexer AppEnvironmentExtra RUST_LOG=shelly_multiplexer=debug
```
