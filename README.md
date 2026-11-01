<div align="center">

# Viherd

**Un environnement de dev local PHP & Node pour Windows, façon Laravel Herd — gratuit, écrit en Rust.**

Sites `*.test`, PHP et Node isolés par projet, workers & queues, WebSockets, bases de données,
dumps, mails et un explorateur de base de données — le tout dans **un seul processus**.

</div>

---

## Pourquoi

Herd est excellent, mais une partie de ses fonctionnalités est payante et il empile plusieurs processus.
Viherd reprend la même expérience en un binaire Rust natif : un proxy HTTP, un pool PHP FastCGI par projet,
un superviseur de workers et des services gérés, pilotés depuis une vraie appli de bureau (WebView2, ~40 Mo de RAM).

## Fonctionnalités

| | |
|---|---|
| **Sites** | `*.test` par dossier parqué ou lié, fichiers statiques, apps Node en proxy (`proxy = 3000`) |
| **PHP & Node isolés** | chaque projet a sa version (`viherd.toml`), son propre `php-cgi` et un `PATH` qui ne contient que ses binaires |
| **Workers & queues** | queue, scheduler, Horizon, Reverb, Pulse, Vite… supervisés, redémarrage auto, détection des crash-loops, logs en direct |
| **Files Laravel** | jobs en attente / en cours / différés / échoués par file (`database` ou `redis`), état d'Horizon, relancer, vider, `queue:restart` |
| **HTTPS local** | `viherd secure` : autorité de certification locale approuvée dans Windows, certificat par site généré à la volée |
| **Partage** | `viherd share` : adresse publique `https://…trycloudflare.com` via Cloudflare Tunnel, sans compte |
| **Xdebug & php.ini** | Xdebug activable par projet (téléchargé au besoin), réglages `php.ini` propres à chaque projet |
| **WebSockets** | proxy WebSocket vers Reverb (`/app`, `/apps`), aussi en `wss://` |
| **Services** | MySQL (MariaDB), PostgreSQL, Redis, RustFS / MinIO (S3), Meilisearch — téléchargés au premier lancement |
| **Explorateur de BDD** | parcourir, filtrer, éditer, insérer, supprimer, éditeur SQL, export CSV, sauvegardes / restauration intégrées |
| **Dumps** | `dump()` / `dd()` capturés avec fichier et ligne, affichés en arbre dépliable (comme Herd) |
| **Mails** | serveur SMTP local qui capture tout, avec aperçu HTML |
| **Journal** | activité HTTP, erreurs, workers et services au même endroit |

## Installation

### Avec l'installeur

Lance **`viherd-setup.exe`** : installation dans `%LOCALAPPDATA%\Programs\Viherd`, sans droits administrateur,
avec la commande `viherd` dans le `PATH`, les raccourcis, le démarrage automatique en option et une entrée
dans « Applications installées » pour désinstaller. Relancer l'installeur met à jour Viherd.

### Depuis les sources

Prérequis : Windows 10/11 et [Rust](https://rustup.rs) (avec les *Visual Studio Build Tools*).

```powershell
git clone https://github.com/Vibesfr/viherd.git
cd viherd
cargo build --release
.\build-installer.ps1        # optionnel : produit target\release\viherd-setup.exe
```

Deux binaires sont produits dans `target\release\` :

- **`viherd-app.exe`** — l'appli de bureau : fenêtre, icône dans la barre des tâches et démon dans le même processus.
  Fermer la fenêtre la cache ; « Quitter Viherd » arrête proprement sites, workers et services.
- **`viherd.exe`** — la CLI (et le démon en mode headless avec `viherd start`). À mettre dans le `PATH`.

> Le port 80 est déjà pris (Herd, IIS…) ? Mets `http_port = 8888` dans `%USERPROFILE%\.viherd\config.toml`.

## Démarrage rapide

```powershell
viherd install php 8.4
viherd install node 22
viherd install composer

cd C:\Users\moi\Sites
viherd park                  # chaque sous-dossier devient <nom>.test

cd mon-app
viherd init                  # crée viherd.toml
viherd use php 8.3           # ce projet utilise PHP 8.3
viherd use node 20

viherd service postgres on   # ou mysql, redis, rustfs, meilisearch…
viherd hosts                 # (admin) ajoute les *.test au fichier hosts
viherd start                 # ou lance viherd-app.exe
```

Dans un projet, ces commandes utilisent automatiquement **sa** version de PHP ou de Node :

```powershell
viherd php -v
viherd artisan migrate
viherd composer install
viherd npm run dev
viherd run <commande>
```

`viherd status` affiche l'état, `viherd reload` recharge les `viherd.toml`.

### HTTPS et partage

```powershell
viherd secure                # https://mon-app.test (le HTTP redirige vers HTTPS)
viherd unsecure
viherd trust                 # approuve seulement l'autorité locale dans Windows
viherd share                 # adresse publique temporaire, Ctrl+C pour arrêter
```

`viherd secure` crée au premier usage une autorité de certification propre à ta machine
(`~/.viherd/certs/ca.crt`) et l'ajoute aux autorités racines de **ton compte** : Windows demande
une confirmation, pas besoin d'être administrateur. Chrome et Edge la reconnaissent aussitôt ;
pour Firefox, active `security.enterprise_roots.enabled` ou importe `ca.crt`. Pense à passer
`APP_URL` en `https://` dans le `.env`.

`viherd share` télécharge `cloudflared` au premier usage et ouvre un tunnel Cloudflare « rapide »
(sans compte). Viherd transmet `HTTPS=on` à PHP pour ces visites : Laravel génère des URL
en `https://…trycloudflare.com` sans configuration.

## Configuration d'un projet — `viherd.toml`

```toml
php  = "8.3"
node = "22"
# root  = "public"   # dossier public (public/ par défaut s'il existe)
# proxy = 3000       # app 100 % Node : tout le site est proxifié

[[routes]]           # WebSockets Reverb : ws://mon-app.test/app → :8080
path = "/app"
port = 8080

[[workers]]
name = "queue"
command = "php artisan queue:work --tries=3"

[[workers]]
name = "reverb"
command = "php artisan reverb:start --host=127.0.0.1 --port=8080"

[env]
# APP_ENV = "local"

[php_ini]            # réglages php.ini propres au projet
# memory_limit = "1G"
```

Ajoute `xdebug = true` (en haut du fichier) pour activer Xdebug sur PHP, les workers et le terminal
du projet : la DLL correspondant à la version de PHP est téléchargée depuis xdebug.org, en mode
`debug,develop` sur le port **9003** (VS Code : extension *PHP Debug*, « Listen for Xdebug »).

Voir [`viherd.example.toml`](viherd.example.toml). Workers, Xdebug et `php.ini` se règlent aussi depuis l'interface.

## Services

| Service | Port | Accès |
|---|---|---|
| MySQL (MariaDB) | 3306 | `root`, sans mot de passe |
| PostgreSQL | 5432 | `postgres`, sans mot de passe |
| Redis | 6379 | — |
| RustFS (S3) | 9000 · console 9001 | `rustfsadmin` / `rustfsadmin` |
| MinIO AIStor (S3) | 9200 | licence à placer dans `~/.viherd/licenses/minio.license` |
| Meilisearch | 7700 | clé `viherd-local-master-key` |

Chaque service a son bloc `.env` prêt à copier dans l'interface. Les arrêts sont propres
(`pg_ctl stop`, `mariadb-admin shutdown`, `redis-cli shutdown`).

## Dumps, mails & logs

Viherd définit `VAR_DUMPER_FORMAT=tcp://127.0.0.1:9912` pour PHP, les workers et le terminal d'un projet :
chaque `dump()` / `dd()` part dans l'onglet **Dumps** au lieu de la page.

Pour capturer les mails, dans le `.env` Laravel :

```env
MAIL_MAILER=smtp
MAIL_HOST=127.0.0.1
MAIL_PORT=2525
```

Logs : `%USERPROFILE%\.viherd\logs`. La page de chaque site affiche aussi son `storage/logs/laravel.log`.

## Architecture

```
viherd-app / viherd start
 ├─ proxy HTTP / HTTPS ── foo.test ──► fichier statique
 │   (rustls, certificat │           ├──────────► pool php-cgi isolé du projet foo (FastCGI)
 │    par site à la volée)           └─ /app (WS) ► Reverb du projet foo
 ├─ cloudflared ─► xxx.trycloudflare.com → foo   (viherd share)
 ├─ API + interface        127.0.0.1:7777
 ├─ superviseur ─► queue:work, schedule:work, reverb, npm run dev…   (par projet)
 ├─ services    ─► mariadbd, postgres, redis-server, rustfs, meilisearch
 ├─ SMTP        127.0.0.1:2525   (capture des mails)
 └─ dumps       127.0.0.1:9912   (Symfony VarDumper)
```

Les binaires de PHP, Node et des services sont stockés une seule fois par version dans `%USERPROFILE%\.viherd`.

## Réglages globaux — `%USERPROFILE%\.viherd\config.toml`

| Clé | Défaut | |
|---|---|---|
| `http_port` | `80` | port des sites |
| `https_port` | `443` | port des sites sécurisés |
| `secured` | `[]` | sites servis en HTTPS (géré par `viherd secure`) |
| `api_port` | `7777` | API et interface |
| `smtp_port` | `2525` | capture des mails |
| `php_processes` | `4` | php-cgi par projet (requêtes en parallèle) |
| `tld` | `"test"` | `"localhost"` évite de toucher au fichier hosts |
| `dumps` | `true` | capture de `dump()` / `dd()` |

## Développer Viherd

```powershell
.\dev.ps1
```

- L'interface (`src/ui.html`) est lue depuis le disque et **se recharge toute seule** à chaque enregistrement.
- Toute modification de `src/*.rs`, `src/*.php` ou `Cargo.toml` arrête proprement l'appli, recompile et la relance.

```
src/
 ├─ app.rs          appli de bureau (tao + wry + tray-icon)
 ├─ main.rs         CLI
 ├─ daemon.rs       orchestration
 ├─ server.rs       proxy HTTP / HTTPS / WebSocket, API
 ├─ certs.rs        autorité locale et certificats par site · share.rs  tunnel Cloudflare
 ├─ redis.rs        client Redis minimal (files Laravel, Horizon)
 ├─ ui.rs           routes de l'API
 ├─ ui.html         interface (un seul fichier, embarqué dans le binaire)
 ├─ php_pool.rs     pool php-cgi par projet · fastcgi.rs
 ├─ supervisor.rs   workers et services
 ├─ services.rs     MySQL, PostgreSQL, Redis, S3, Meilisearch
 ├─ db.rs           explorateur et sauvegardes
 ├─ dumps.rs        serveur de dumps · mail.rs  SMTP
 └─ versions.rs     installation de PHP / Node / Composer / Xdebug
installer/          viherd-setup.exe : fenêtre WebView sans bordure, interface dans installer/src/ui.html
```

`cargo test` lance les tests unitaires ; la CI GitHub Actions compile et teste sous Windows
et publie `viherd-setup.exe` en artefact.

L'interface de l'installeur est une simple page HTML : pour la retoucher, ouvre
`installer/src/ui.html` dans un navigateur (les boutons s'y affichent dans la console),
puis `.\build-installer.ps1`.

---

Projet personnel, non affilié à Laravel ni à Beyond Code.
