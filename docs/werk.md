# Eigene Apps auf traum-haft (werk)

Für Entwickler: Jede Anwendung mit einem Container-Image läuft intern unter
`https://<name>.werk.isp-insoft.de`, hinter dem Firmen-Login. Kein Ticket,
kein Admin: drei Dateien ins Repository, `git push`, fertig.

> Stand: Entwurf. Beschreibt den geplanten Ablauf; noch nicht in Betrieb.

## Voraussetzungen

- Repository in der GitHub-Organisation `isp-insoft-gmbh`
- Die App läuft als Container und spricht HTTP auf einem Port

## In drei Dateien online

### 1. `traum-haft.toml` (Wurzel des Repositorys)

```toml
name   = "datacloak"                  # Adresse: https://datacloak.werk.isp-insoft.de
port   = 8080                         # Port im Container; steht auch in $PORT
owners = ["vorname.nachname@isp-insoft.de"]

# optional
health       = "/"                    # muss nach dem Start mit 2xx/3xx antworten (Standard "/")
memory       = "512m"                 # Standard 512m, höchstens 4g
secrets      = ["OPENAI_API_KEY"]     # GitHub-Secrets, die als Umgebungsvariable ankommen
integrations = ["jira:read"]          # Firmendienste, siehe unten
```

`name`: Kleinbuchstaben, Ziffern, einzelne Bindestriche, höchstens 40
Zeichen. Wer zuerst kommt, bekommt den Namen.

`owners`: Diese Personen sehen Logs und bekommen Meldungen. Mindestens eine.

### 2. `Containerfile` (oder `Dockerfile`)

Beliebiger Stack. Beispiel für eine Java-App:

```dockerfile
FROM docker.io/library/eclipse-temurin:25-jre
COPY target/app.jar /app/app.jar
CMD ["java", "-jar", "/app/app.jar"]
```

Die App muss auf `0.0.0.0` lauschen (nicht `127.0.0.1`), auf dem Port aus
`port` bzw. `$PORT`.

### 3. `.github/workflows/traum-haft.yml`

Unverändert übernehmen:

```yaml
name: traum-haft
on:
  workflow_dispatch:
    inputs:
      sha: { required: true }
      version: { required: true }
jobs:
  build:
    uses: bugabinga/traum-haft/.github/workflows/build-container.yml@main
    with:
      sha: ${{ inputs.sha }}
      version: ${{ inputs.version }}
    secrets: inherit
```

Die Plattform startet diesen Workflow selbst; von Hand ist nichts auszulösen.

### Veröffentlichen

`git push` auf `main`. Nach wenigen Minuten:

- läuft die neue Version unter `https://<name>.werk.isp-insoft.de`,
- zeigt der Commit auf GitHub den Status `traum-haft` mit Link zu den Logs.

Jeder Push auf `main` ist eine neue Version. Startet die neue Version nicht
(Health-Check schlägt fehl), bleibt die alte online.

## Was die App bekommt

**Nur angemeldete Besucher.** Jede Anfrage kommt von jemandem mit
`@isp-insoft.de`-Google-Konto. Die App muss keinen Login bauen. Wer es ist,
steht in den Headern:

| Header | Inhalt |
|---|---|
| `X-User-Email` | `vorname.nachname@isp-insoft.de` |
| `X-User-Sub` | feste Google-Kennung der Person |

Den Headern ist zu trauen: Die App ist nur über den Plattform-Eingang
erreichbar, der sie setzt und vom Browser gesendete überschreibt.

**Umgebungsvariablen**

| Variable | Inhalt |
|---|---|
| `PORT` | der Port aus `traum-haft.toml` |
| `TRAUM_HAFT_APP` | der Name |
| `TRAUM_HAFT_URL` | `https://<name>.werk.isp-insoft.de` |
| je Eintrag in `secrets` | der Wert des gleichnamigen GitHub-Secrets |

**Speicher:** `/data` bleibt über Versionen hinweg erhalten, nur für diese
App. Alles andere im Container ist nach dem nächsten Deploy weg. Noch keine
Sicherung: Wichtiges selbst sichern.

**Internet:** ausgehend frei. Interne Server der Plattform sind gesperrt.

**Grenzen:** 1 CPU, Arbeitsspeicher laut `memory`.

## Secrets

1. Auf GitHub: Repository → Settings → Secrets and variables → Actions →
   New repository secret, z. B. `OPENAI_API_KEY`.
2. Den Namen in `secrets` eintragen.
3. Pushen. Der Wert kommt verschlüsselt bei der App an; nur der Server der
   Plattform kann ihn lesen.

Geändertes Secret: einmal pushen (auch ein leerer Commit genügt).

## Firmendienste (optional)

Jira, Tempo und CRM Plus ruft die App im Browser über die Plattform auf, als
der angemeldete Besucher und mit dessen Rechten. Tokens sieht die App nie.

1. In `traum-haft.toml`: `integrations = ["jira:read", "tempo:worklogs"]`
2. Im Browser-Code:

```js
const res = await fetch("/_api/jira/rest/api/3/myself");
if (res.status === 401) {
  const { connect_url } = await res.json();
  location.href = connect_url;   // Besucher verbindet Jira einmal, kommt zurück
}
```

Verfügbare Dienste und Bereiche: siehe `https://apps.isp-insoft.de/llms.txt`,
Abschnitt Integrations.

## Logs

`https://werk.isp-insoft.de/logs/<name>`: Ausgabe der App (stdout/stderr) und
Plattformmeldungen (Build, Start, Health-Check). Nur für `owners`.

Logs enthalten, was die App ausgibt: keine Passwörter, Tokens oder
Kundendaten loggen.

## Zurück, abschalten

- **Alte Version:** `git revert` des fehlerhaften Commits, pushen.
- **Abschalten:** `traum-haft.toml` löschen oder das Repository archivieren.
  Die App geht offline; `/data` bleibt 30 Tage erhalten.

## Lokal testen

```bash
podman build -t meine-app .
podman run --rm -p 8080:8080 -e PORT=8080 meine-app
curl -H 'X-User-Email: test@isp-insoft.de' -H 'X-User-Sub: 1' http://localhost:8080/
```

## Wenn es nicht klappt

| Status am Commit | Ursache | Abhilfe |
|---|---|---|
| `Name vergeben` | `name` gehört schon einer anderen App | anderen Namen wählen |
| `Workflow fehlt` | Datei 3 fehlt oder heißt anders | `.github/workflows/traum-haft.yml` anlegen |
| `Build fehlgeschlagen` | Containerfile baut nicht | Log am Commit lesen, lokal `podman build` |
| `Start fehlgeschlagen` | Health-Check bekommt keine Antwort | lauscht die App auf `0.0.0.0:$PORT`? Logs lesen |
| `traum-haft.toml ungültig` | Tippfehler, fehlendes Feld | Meldung am Commit nennt das Feld |

Keine Reaktion nach einem Push: Ist das Repository in `isp-insoft-gmbh` und
liegt `traum-haft.toml` auf `main`?

## Mit Claude Code

Die Agent-Fassung dieser Anleitung liegt unter
`https://werk.isp-insoft.de/llms.txt`. Claude Code damit anweisen genügt:
„Mach diese App bereit für traum-haft werk.“
