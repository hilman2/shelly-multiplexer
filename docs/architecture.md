# ShellyMultiplexer — Architektur

## Problem

Mehrere Akku-Speicher (Marstek B2500/Venus, Hoymiles MS-A2, Anker Solix, …) regeln einzeln gegen denselben Shelly 3EM. Da kein Speicher von den anderen weiß, reagieren alle gleichzeitig auf jeden Bezugs-/Einspeisewert. Folgen:

- N-fache Reaktion auf jede Lastveränderung
- Überschwinger in beide Richtungen
- Schwingung nimmt mit jedem zusätzlichen Akku zu
- Keine Lastverteilung nach Sicherungsgruppe (B16 etc.)
- Keine Priorisierung nach SoC

## Lösung in einem Satz

Ein **virtueller Shelly 3EM pro Akku**, der jedem Akku nur seinen anteiligen Bezugs-/Einspeisewert vorspielt. Summe aller Anteilswerte = realer Wert.

## Topologie / Portbelegung

```
   Realer Shelly 3EM Pro
   ─────────────────────
   UDP-RPC Port 2020   (umkonfiguriert, weg vom Standard 1010)
        ▲
        │ Poller pollt 2–5 Hz
        │
   ┌────┴──────────────────────────────────────────┐
   │  ShellyMultiplexer                            │
   │                                               │
   │  ┌────────────┐   ┌────────────┐              │
   │  │   Poller   │──▶│ Dispatcher │              │
   │  │ (real EM)  │   │ + Caps     │              │
   │  └────────────┘   └─────┬──────┘              │
   │                         │ Anteils-Tabelle     │
   │                         │ (pro Akku-IP)       │
   │                         ▼                     │
   │  ┌──────────────────────────────────────┐     │
   │  │ Virtual Shelly Pro 3EM (Server)      │     │
   │  │  • UDP-RPC  :1010                    │     │
   │  │  • HTTP-RPC :80    /rpc              │     │
   │  │  • REST     :80    /shelly /status   │     │
   │  │  • mDNS     _shelly._tcp             │     │
   │  └──────────────────────────────────────┘     │
   └───────────┬───────────┬───────────┬───────────┘
               │           │           │
   ┌───────────▼─┐ ┌───────▼────┐ ┌────▼────────┐
   │ Marstek 1   │ │ Marstek 2  │ │ Hoymiles 1  │
   │ 192.168.1.51│ │.52         │ │.61          │
   └─────────────┘ └────────────┘ └─────────────┘
```

**Kein NAT, kein Source-IP-Spoofing, keine IP-Aliase.** Alle Akkus pollen den gleichen Endpunkt; der Dispatcher unterscheidet sie über die Source-IP.

## Kern-Insight aus uni-meter

`uni-meter` (Apache-2.0, [github.com/sdeigm/uni-meter](https://github.com/sdeigm/uni-meter)) emuliert bereits einen Shelly Pro 3EM und hat einen **per-Client-Skalierungsfaktor** eingebaut, der ursprünglich nur für Power-Factor-Korrekturen gedacht war:

`reference/uni-meter/OutputDevice.java:565`
```java
protected double getPowerFactorForRemoteAddress(@NotNull InetAddress remoteAddress) {
  ClientContext clientContext = clientContexts.get(remoteAddress);
  if (clientContext != null && clientContext.powerFactor() != null) {
    return clientContext.powerFactor();
  }
  return defaultClientPowerFactor;
}
```

`reference/uni-meter/ShellyPro3EM.java:1196` (rpcEmGetStatus)
```java
double totalPower = (powerPhase0.power() + powerPhase1.power() + powerPhase2.power()) * factor;
// ...
powerPhase0.power() * factor, powerPhase1.power() * factor, powerPhase2.power() * factor, ...
```

→ **Genau die Mechanik die wir brauchen.** Die Antwort wird pro Source-IP individuell skaliert. Statt eines statischen `power-factor` aus der Config muss der Faktor dynamisch vom Dispatcher kommen.

## Komponenten

### 1. Real-EM Poller
- Pollt den echten Shelly 3EM (auf umkonfiguriertem Port 2020) per UDP-RPC `EM.GetStatus` und `EMData.GetStatus`
- Frequenz: 2–5 Hz (also schneller als die Akkus pollen, damit Akkus immer aktuelle Daten sehen)
- Schreibt das Ergebnis in einen geteilten `LatestEmStatus`-Snapshot (atomare Ablage, lock-free Read)

### 2. Dispatcher
Berechnet bei jedem neuen Snapshot vom Poller die Anteilstabelle. Output: `Map<InetAddress, AllocationFactor>`.

**Eingaben:**
- aktueller Snapshot vom realen Shelly (pro Phase: P, V, I, S, PF, f)
- Konfigurierte Akku-Liste mit Gruppen-Zugehörigkeit, Phase, Nennleistung, Priorität
- Live-State: SoC pro Akku (sofern abrufbar; sonst priorität-basiert)
- Gruppen-Caps (B16 = 3680 W single phase, 11040 W three phase)

**Algorithmus (zweistufig):**

```
Phase 1 — Gruppenanteil:
  für jede Gruppe g:
    g.anteil = clamp(gewicht[g] * gesamtbedarf, -g.discharge_cap, g.charge_cap)
  rest = gesamtbedarf - sum(g.anteil)
  rest auf nicht-gesättigte Gruppen verteilen (iterativ bis fixpunkt oder rest=0)

Phase 2 — Akku-Anteil innerhalb Gruppe:
  für jede Gruppe g:
    für jeden Akku a in g:
      a.anteil = gewicht[a, soc, prio] * g.anteil
      a.anteil clampen auf [-a.max_discharge, a.max_charge]
    rest innerhalb g auf nicht-gesättigte Akkus verteilen

Per Akku speichern:
  factor[a.ip] = a.anteil / gesamtbedarf      (saldiert)
  oder phasenweise:
  factor[a.ip][phase] = a.anteil[phase] / phase.power   (wenn phasen-bewusst)
```

**Wichtig:** Der Faktor multipliziert **alle Phasen einheitlich** (wie uni-meter es tut), oder phasenweise wenn der Akku einphasig auf einer bestimmten Phase hängt. Letzteres erfordert eine Erweiterung von `rpcEmGetStatus` damit pro Phase ein eigener Faktor angewandt wird.

### 3. Anteils-Tabelle (Cache)
- `ConcurrentMap<InetAddress, AllocationFactor>`
- Wird vom Dispatcher geschrieben, vom RPC-Server gelesen
- TTL pro Eintrag (z.B. 30 s) — wenn ein Akku lange nichts pollt, wird er beim nächsten Recompute ignoriert (vermeidet Geister-Allokationen)

### 4. Virtual Shelly Pro 3EM Server
**Direkter Fork des uni-meter ShellyPro3EM-Actors.** Änderungen:

| Datei | Änderung |
|---|---|
| `OutputDevice.getPowerFactorForRemoteAddress()` | statt aus `clientContext.powerFactor()` aus Dispatcher-Cache lesen |
| `OutputDevice.eventPowerDataChanged()` | wird vom Poller statt von einem Input-Device getriggert |
| Input-Devices ganzer Subtree | entfällt — Poller ersetzt das |
| Config-Schema | erweitert um `groups`, `batteries[]` mit `address`, `group`, `priority` etc. |
| `ShellyPro3EM.rpcEmGetStatus()` | optional: pro-Phase-Faktor unterstützen |

Beibehalten:
- UDP-Server (`UdpServer.java`, `UdpBindFlow.java`) — funktioniert wie ist
- HTTP-Routes (`HttpRoute.java`)
- WebSocket-Input/Output
- mDNS-Registrierung
- Throttling-Queues (`min-sample-period`)
- Komplettes RPC-Mapping in `Rpc.java`
- Alle Status-Records (Cloud, Wifi, Sys, Eth, Modbus, …)

### 5. Konfiguration
HOCON (wie uni-meter). Skizze:

```hocon
shelly-multiplexer {
  http-server.port = 80
  
  real-shelly {
    host = "192.168.1.50"
    udp-port = 2020          # ← echter Shelly muss umkonfiguriert sein
    poll-interval = 250ms
  }
  
  output {                   # virtueller Shelly Pro 3EM (übernommen von uni-meter)
    interface = "0.0.0.0"
    port = 80
    udp-interface = "0.0.0.0"
    udp-port = 1010
    min-sample-period = 1000ms
    device {
      mac = ""               # leer = autodetect
      hostname = ""
    }
    # cloud, ws, fw, ... wie uni-meter
  }
  
  groups = [
    {
      id = "keller"
      fuse-amps = 16
      phases = 1             # 1 oder 3
      voltage = 230
      # → cap = 3680 W
    }
    {
      id = "garage"
      fuse-amps = 16
      phases = 3
      # → cap = 11040 W
    }
  ]
  
  batteries = [
    {
      address = "192.168.1.51"
      vendor = "marstek"
      group = "keller"
      phase = "a"            # a/b/c bei einphasigen Akkus, "all" bei 3-phasigen
      max-charge-w = 2500
      max-discharge-w = 2500
      priority = 1
    }
    {
      address = "192.168.1.52"
      vendor = "marstek"
      group = "keller"
      phase = "a"
      max-charge-w = 2500
      max-discharge-w = 2500
      priority = 1
    }
    {
      address = "192.168.1.61"
      vendor = "hoymiles"
      group = "garage"
      phase = "all"
      max-charge-w = 1920
      max-discharge-w = 1920
      priority = 2
    }
  ]
  
  dispatcher {
    weighting = "equal"      # equal | by-soc | by-capacity | priority-tiered
    rate-limit-watts-per-second = 500   # Anti-Schwing-Dämpfung
  }
}
```

## Datenfluss (Sequenz)

```
t=0.000  Poller --UDP--> echter Shelly:2020   "EM.GetStatus"
t=0.020  echter Shelly --UDP--> Poller         {a_act_power: 800, b: 1100, c: 1100, total: 3000}
t=0.021  Snapshot atomar in LatestEmStatus geschrieben
t=0.022  Dispatcher recompute:
            Gruppe "keller" (Phase a, cap 3680 W) → 800 W zugeteilt
              Akku 51, 52 je 400 W
            Gruppe "garage" (3-phasig, cap 11040 W) → restliche 2200 W
              Akku 61 → 2200 W
            Faktoren: 51→0.5, 52→0.5 (von Phase a),  61→0.733 (von gesamt)

t=0.100  Marstek 51 --UDP--> Multiplexer:1010  "EM.GetStatus" {src:"marstek-51", id:7}
t=0.101  Multiplexer Antwort: a_act_power=400, b_act_power=0, c=0, total=400  (Faktor 0.5 nur auf Phase a)
t=0.150  Marstek 52 fragt → analog 400 W
t=0.180  Hoymiles 61 fragt → 800*0.733=586, 1100*0.733=807, 1100*0.733=807, total=2200
...
t=0.250  Poller liest erneut → 1500 W (Last gefallen, Akkus reagieren bereits)
         Dispatcher recompute → neue Faktoren
t=0.380  nächster Marstek-Poll bekommt neue Werte
```

## Anti-Schwing-Strategie

Selbst mit korrekter Aufteilung kann das System schwingen wenn alle Akkus identisch P-regeln. Maßnahmen im Dispatcher:

1. **Rate-Limiting:** maximale Änderung der zugeteilten Leistung pro Sekunde (z.B. 500 W/s pro Akku)
2. **Totzone:** kleine Bezugs-/Einspeisewerte (< 50 W) nicht aufteilen, bei null lassen
3. **Hysteresis:** Vorzeichenwechsel der Aufteilung erst nach Verharren > 2 s
4. **Asymmetrische Reaktion:** Lade-Anstieg langsam, Entlade-Anstieg schnell (umgekehrt für Sicherheit)

## Implementierungs-Strategie

### Variante A — Fork von uni-meter (empfohlen)
1. `uni-meter`-Repo forken/extrahieren in `src/`
2. Neues Input-Device `RealShellyPoller` schreiben (eigentlich nur eine Variante des bestehenden `ShellyPro3EM`-Input-Adapters mit anderem Port und höherer Frequenz)
3. `OutputDevice.getPowerFactorForRemoteAddress()` ersetzen durch Dispatcher-Lookup
4. Dispatcher als neuer Pekko-Actor neben `OutputDevice`
5. Config-Schema in `application.conf` erweitern
6. Build-Tooling übernehmen (Maven, Java 17, Docker)

**Vorteile:** ~80% des Codes existiert. Wire-Format ist erprobt mit Marstek/Hoymiles. Apache-2.0-Lizenz erlaubt Fork ohne Einschränkungen.

**Nachteile:** Java/Pekko ist schwergewichtig (Akka/Pekko-Actor-System, Maven, JVM). RAM-Footprint ~150 MB.

### Variante B — Neuimplementierung (z.B. Python asyncio oder Go)
Wenn Footprint kritisch (Raspi Zero, ESP) oder andere Sprache präferiert. Dann gilt:
- uni-meter als **Wire-Protokoll-Referenz** verwenden
- Mit Wireshark Marstek↔Shelly mitschneiden, gegen `ShellyPro3EM.java`/`Rpc.java` abgleichen
- Mindestens implementieren: `EM.GetStatus`, `EMData.GetStatus`, `EM.GetConfig`, `Shelly.GetDeviceInfo`, `Shelly.GetStatus`, `Shelly.GetConfig`, `Shelly.GetComponents`, `Sys.GetStatus`

**Empfehlung:** Variante A für die erste lauffähige Version, evtl. später Reimplementierung in leichterer Sprache wenn die Logik validiert ist.

## Offene Fragen / TODOs

- [ ] **Welche Akkus konkret?** Liste fixieren → bestimmt welche RPC-Methoden zwingend sind
- [ ] **mDNS-Konflikt:** Wenn echter Shelly weiterhin per mDNS sichtbar bleibt, könnten Akkus den finden statt den Multiplexer. Lösung: echter Shelly im VLAN/Subnetz isoliert, oder mDNS am echten Shelly deaktiviert
- [ ] **Wire-Capture machen:** Bevor wir starten 1× Marstek↔Shelly mit Wireshark mitschneiden, um sicher zu sein dass die uni-meter-Implementierung wirklich alle Felder abdeckt die der Marstek erwartet
- [ ] **SoC-Quelle pro Hersteller:** Marstek hat lokale API/MQTT, Hoymiles hat Cloud — wie holen wir SoC für gewichtete Aufteilung?
- [ ] **Failover:** Was wenn Multiplexer crasht? Watchdog → Service-Restart oder direktes Routing zum echten Shelly als Fallback
- [ ] **Phasen-bewusste Aufteilung:** uni-meter wendet einen Skalar auf alle 3 Phasen an. Für einphasige Akkus (Marstek auf L1) brauchen wir pro-Phase-Faktoren — wie groß ist der Code-Eingriff?

## Nächste Schritte (konkret)

1. Marstek↔Shelly Wireshark-Capture machen
2. Akku-Inventar fixieren (Modelle, IP-Adressen, Phasenzuordnung, Gruppen)
3. Variante A: uni-meter forken, Build lokal hochziehen, "Hello-World"-Lauf mit 1 Akku ohne Aufteilung (Faktor=1.0) → Beweis dass Akku den emulierten Shelly akzeptiert
4. Dispatcher-Stub mit konstanten Faktoren (z.B. `1/N`) → Beweis dass Aufteilung funktioniert
5. Echte Dispatcher-Logik mit Gruppen-Caps
6. Anti-Schwing-Dämpfung
7. Monitoring / Web-UI (übernimmt uni-meter größtenteils per HTTP `/status`)

## Referenz-Quellcode (lokal)

`reference/uni-meter/` enthält die kritischen Original-Dateien aus `sdeigm/uni-meter`:

- [UdpServer.java](../reference/uni-meter/UdpServer.java) — Pekko UDP-Source/Sink-Setup
- [UdpBindFlow.java](../reference/uni-meter/UdpBindFlow.java) — UDP-Bind-Stage
- [Shelly.java](../reference/uni-meter/Shelly.java) — Basisklasse, Client-Context, Settings
- [ShellyPro3EM.java](../reference/uni-meter/ShellyPro3EM.java) — vollständige Pro-3EM-Emulation, **das Template**
- [HttpRoute.java](../reference/uni-meter/HttpRoute.java) — HTTP-Routen (`/rpc`, `/shelly`, `/settings`, WebSocket-Upgrade)
- [Rpc.java](../reference/uni-meter/Rpc.java) — Wire-Format aller RPC-Records (Request/Response/Notification)
- [OutputDevice.java](../reference/uni-meter/OutputDevice.java) — `getPowerFactorForRemoteAddress`, `PowerData`, ClientContext-Verwaltung
