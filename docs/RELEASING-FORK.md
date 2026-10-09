# Publicar LiMusic Forge

Guía para quien publica builds de este fork (`Kushro/limusic-forge`). Todo lo que sigue es
**opcional**: un build sin ninguno de estos secretos compila y funciona; sólo pierde la función
correspondiente (ver la tabla del final). Ningún valor real aparece en este repositorio: cada
clave la genera o registra quien publica, y vive en sus secretos de GitHub o en su máquina.

## 1. Clave de firma del actualizador (minisign)

El actualizador de Tauri sólo instala paquetes firmados con la clave privada cuya clave pública
está en `src-tauri/tauri.conf.json`. Upstream firma con su propia clave, que el fork no tiene. El
fork ya tiene la suya: `plugins.updater.pubkey` lleva la pública de `~/.tauri/limusic-forge.key`
(ID de clave `9A4AC49B4D3C2FD3`), sin contraseña.

Si `pubkey` quedara vacío, `can_self_update` devuelve `false` y la app no se actualiza sola:
cuando hay una versión nueva, el aviso ofrece **descargarla** desde la página de releases.

Para generar o reemplazar la clave:

1. Generá el par de claves (una sola vez, y guardá la privada fuera del repo):

   ```sh
   cargo tauri signer generate -w ~/.tauri/limusic-forge.key
   ```

   El comando pide una contraseña (puede quedar vacía) y escribe `limusic-forge.key` (privada) y
   `limusic-forge.key.pub` (pública).
2. Copiá el contenido de `limusic-forge.key.pub` en `src-tauri/tauri.conf.json`, en
   `plugins.updater.pubkey`, y commitealo. La pública no es secreta.
3. En GitHub, *Settings ▸ Secrets and variables ▸ Actions*, creá:
   - `TAURI_SIGNING_PRIVATE_KEY`: el contenido de `limusic-forge.key`.
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: la contraseña elegida (vacía si no pusiste ninguna).

Si perdés la clave privada, los usuarios con un build firmado por ella no pueden recibir
actualizaciones automáticas de builds firmados con una clave nueva: tienen que descargar e
instalar a mano una vez.

El endpoint ya apunta al fork:
`https://github.com/Kushro/limusic-forge/releases/latest/download/latest.json`.

## 2. Discord Rich Presence

El App ID de Discord se fija **al compilar**, desde la variable de entorno
`LIMUSIC_DISCORD_APP_ID` (`option_env!` en `src-tauri/src/discord.rs`). Si falta o está vacía,
Rich Presence queda deshabilitado: la pestaña Discord de Ajustes y el botón de la barra de título
muestran "no configurado en este build".

1. En el [Discord Developer Portal](https://discord.com/developers/applications) creá una
   aplicación llamada **LiMusic Forge**. El nombre es lo que Discord muestra después de
   "Listening to", y su ícono es la carátula de respaldo. No hace falta bot, redirect OAuth ni
   client secret.
2. Copiá su **Application ID** (sólo dígitos). El App ID es público: viaja dentro del binario.
3. Para los builds de CI, creá el secreto de Actions `LIMUSIC_DISCORD_APP_ID` con ese número. Los
   tres workflows de release (`windows-`, `linux-` y `macos-release.yml`) lo pasan al paso de
   build.
4. Para compilar en local, exportá la variable antes de `cargo` / `cargo tauri build`:

   ```sh
   LIMUSIC_DISCORD_APP_ID=<tu App ID> cargo tauri build
   ```

   En PowerShell: `$env:LIMUSIC_DISCORD_APP_ID = '<tu App ID>'`. `build.rs` declara la variable
   con `rerun-if-env-changed`, así que cambiarla recompila lo necesario.

El test `app_id_is_empty_or_a_snowflake` falla si la variable tiene algo que no sea un App ID.

## 3. Last.fm

Last.fm exige una cuenta de API propia: <https://www.last.fm/api/account/create>. Te da una
*API key* y un *shared secret*.

- **En local:** creá `src-tauri/lastfm.keys` (está en `.gitignore`; nunca lo commitees) con dos
  líneas, en el formato que lee `src-tauri/build.rs`:

  ```text
  LIMUSIC_LASTFM_API_KEY=<tu API key>
  LIMUSIC_LASTFM_API_SECRET=<tu shared secret>
  ```

- **En CI:** creá los secretos de Actions `LASTFM_API_KEY` y `LASTFM_API_SECRET`. Cada workflow
  de release escribe con ellos el mismo `lastfm.keys` antes de compilar; si faltan, emite un
  *warning* y el build sale sin Last.fm.

Sin claves, `lastfm_status` informa `configured: false` y conectar devuelve un error que remite a
este documento.

## 4. Escuchar juntos (Listen Together)

El fork **no trae servidor predeterminado** (`DEFAULT_SERVER` está vacío en
`src-tauri/src/listentogether/mod.rs`): el de upstream es una máquina ajena. Sin servidor, el
panel de Escuchar juntos pide la dirección como campo obligatorio, y el backend rechaza conectarse
con un aviso.

Para tener uno, corré el servidor del propio repo, `crates/sync-server` (binario `limusic-sync`):

```sh
cargo run --release -p sync-server
# o con otro puerto:
PORT=9000 cargo run --release -p sync-server
```

Escucha `ws://` en `0.0.0.0:$PORT` (8080 por defecto) y no hace TLS: ponelo detrás de algo que
termine TLS (Tailscale Funnel, Cloudflare Tunnel, un proxy inverso). En la app, la dirección va
como `wss://<tu-host>/ws`. Las invitaciones de una sala incluyen el servidor (`CODIGO@<tu-host>`),
así que los invitados no tienen que configurarlo.

## 5. Procedimiento de release

1. Subí la versión en `Cargo.toml` (workspace) y `src-tauri/tauri.conf.json`; tienen que
   coincidir (el test `brand_identity_is_forge` lo comprueba).
2. Esquema de versiones (decisión D1):
   - `X.Y.Z-forge.N` es una versión **estable** del fork: se publica como *Latest* y su
     `latest.json` queda en `/releases/latest/download/`.
   - Sólo un sufijo que empieza por `rc`, `beta` o `alpha` (`1.3.0-rc.1`) es *prerelease*: va
     al canal beta y nunca es *Latest*. Cortá los RC sobre la próxima versión de upstream
     (`1.3.0-rc.N`), no como `1.2.0-forge.2-rc.1`: ese sufijo empieza por `forge`, así que cuenta
     como estable y además ordena después de `1.2.0-forge.2`.
   - La app aplica la misma regla (`is_prerelease` en `commands.rs`, `isPrerelease` en
     `ui/src/lib/version.ts`): las notas de versión y el aviso de actualización tratan `-forge.N`
     como estable.
3. Creá el tag `vX.Y.Z-forge.N` sobre el commit publicado en `master` y el release en GitHub, y
   lanzá los workflows de release con ese tag (*Actions ▸ … ▸ Run workflow*). Los workflows son
   sólo `workflow_dispatch`: publicar un release desde la web no compila nada.
4. Cada workflow adjunta sus binarios y su entrada en `latest.json`; el release pasa a *Latest*
   cuando las plataformas están completas.

`bash scripts/release.sh` hace los pasos 3 y 4 de una vez: publica el release en
`Kushro/limusic-forge` y lanza los tres workflows con el tag. La CI y el script usan el mismo
criterio de prerelease que la app (una función `is_prerelease` idéntica en `release.sh` y en los
tres workflows): se descarta el build metadata (`+…`), se toma el sufijo tras el **primer** `-`, y
la versión es prerelease sólo si ese sufijo **empieza** por `rc`, `beta` o `alpha`, sin distinguir
mayúsculas. Así `1.3.0-rc.1`, `1.2.0-RC.1` y `1.3.0-beta.2+b.5` son prerelease, y `1.2.0`,
`1.2.0-forge.1` y `1.2.0-forge.2-rc.1` salen como estables.

Los scripts se invocan con bash (`bash scripts/…`); GitButler en Windows no conserva el bit +x.
El job `rustfmt` de `checks.yml` falla si un workflow llama a `scripts/*.sh` sin `bash` delante.

Runners: `checks.yml`, `linux-release.yml`, `windows-release.yml` y `stream-health.yml` eligen el
runner con `.github/workflows/runner.yml`. La variable de repositorio `RUNNER_PROVIDER` decide:
`avrea` fuerza Avrea, `github` fuerza los runners de GitHub, y sin definir (o `auto`) usa Avrea
salvo que su página de estado reporte una caída mayor o no responda (los PR desde forks van siempre
a GitHub). `macos-release.yml` queda siempre en `macos-14` de GitHub. Un `dry_run` de los workflows
de release compila con `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` para validar el empaquetado más
rápido; los releases publicados mantienen `codegen-units = 1`.

## 6. Qué pasa si falta cada cosa

| Falta | Efecto en el build | Cómo se ve en la app |
|---|---|---|
| `plugins.updater.pubkey` vacío | No hay actualización automática (`can_self_update = false`) | El aviso de versión nueva ofrece descargarla desde GitHub |
| `TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD` | Los workflows de release fallan a propósito: no hay `.sig` ni `latest.json` firmado | Nada que publicar |
| `LIMUSIC_DISCORD_APP_ID` | Rich Presence deshabilitado (`discord_available = false`) | Pestaña Discord y botón de la barra de título: "no configurado en este build" |
| `LASTFM_API_KEY` / `LASTFM_API_SECRET` (CI) o `src-tauri/lastfm.keys` (local) | Sin scrobbling (`configured: false`); la CI sólo avisa | Conectar Last.fm muestra un error que remite a este documento |
| Servidor de Escuchar juntos | No hay servidor predeterminado | El panel pide la dirección del servidor; sin ella no se puede crear ni unirse a una sala |
