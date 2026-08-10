# Ferrite

Un clone de PostgreSQL en 100 % Rust — plus léger et plus rapide que Postgres en coupant délibérément les fonctionnalités à faible usage/forte complexité plutôt qu'en visant la parité complète.

Voir [`docs/architecture.md`](docs/architecture.md) pour le plan complet : ce qui est gardé, ce qui est coupé et pourquoi, le modèle de sécurité (identité + procédures plutôt qu'un DSL de policy RLS séparé), le découpage en crates, et l'ordre de dépendance entre elles.

## Structure

```
crates/
  ferrite-common/     types + traits partagés (aucune implémentation)
  ferrite-metrics/    registre Prometheus + endpoint HTTP /metrics et /health
  ferrite-storage/    pages, B-tree, MVCC, journal de récupération
  ferrite-catalog/    catalogue système
  ferrite-sql/        lexer/parser -> AST
  ferrite-planner/    AST -> plan logique -> plan physique (à règles)
  ferrite-exec/       exécuteur
  ferrite-proc/       triggers, procédures stockées, sécurité par identité
  ferrite-protocol/   protocole PostgreSQL v3, TLS, auth
  ferrite-server/     binaire, écoute sur 5432
```

## Développement

```
cargo build --workspace
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

## Docker

```
docker build -t ferrite-server .
docker run -p 5432:5432 ferrite-server
```

Image publiée sur `ghcr.io/mairie-creusot/ferrite` (privé) à chaque push sur `main` touchant le code.

## Production

### Politique de redémarrage

**Une image ne peut pas se redémarrer toute seule.** La politique de
redémarrage n'existe pas dans le `Dockerfile`, elle se déclare au
lancement — sans elle, un process qui meurt reste mort.

```bash
docker run -d --name ferrite \
  --restart=unless-stopped \
  -p 5432:5432 \
  -e FERRITE_PASSWORD='...' \
  -v ferrite-data:/data \
  ghcr.io/mairie-creusot/ferrite:latest
```

`unless-stopped` plutôt qu'`always` : un `docker stop` volontaire de
l'opérateur reste respecté au redémarrage du démon Docker, une panne non.

**Ce que la politique couvre, et ce qu'elle ne couvre pas** (mesuré sur
Docker 29.6.2) : elle relance le conteneur quand le process se termine de
lui-même — panique, `abort`, OOM killer, sortie non nulle. Elle ne le
relance **pas** après un `docker kill` ni un `docker stop` : Docker les
enregistre comme un arrêt volontaire de l'opérateur et suspend la
politique, le conteneur reste à terre avec `RestartCount` à 0. C'est le
comportement voulu, mais il rend `docker kill` inutilisable pour tester
qu'un redémarrage automatique fonctionne — voir
`crates/ferrite-server/tests/stress.rs`.

Le plus simple reste [`docker-compose.yml`](docker-compose.yml), qui porte
la politique, le volume, le healthcheck et les variables d'environnement au
même endroit :

```bash
echo "FERRITE_PASSWORD=$(openssl rand -base64 24)" > .env
docker compose up -d
```

`FERRITE_PASSWORD` n'est pas optionnel en production : sans lui le serveur
génère un mot de passe aléatoire **à chaque démarrage** et le journalise,
donc les clients ne se reconnectent pas après un redémarrage. Pareil pour
le volume : sans `-v`, les données vivent dans la couche écrivable du
conteneur et disparaissent avec lui.

### Healthcheck

Le `HEALTHCHECK` de l'image interroge `GET /health` sur le port
d'observabilité, pas le port SQL. La différence est le point important :
un `nc -z 127.0.0.1 5432` ne prouve que l'existence du listener, et un
serveur dont le moteur est bloqué (verrou de stockage tenu, disque plein,
thread bloquant parti) accepte toujours la connexion TCP. `/health` fait un
vrai aller-retour moteur — catalogue lu, transaction ouverte et validée,
donc `fsync` du journal — sous une échéance de 3 s, et rend `503` avec la
raison sinon.

| Réponse | Sens |
| --- | --- |
| `200 ok` | le moteur a répondu ; la durée de la sonde est dans le corps |
| `503 unhealthy` + raison | le moteur a répondu une erreur (disque plein, verrou empoisonné…) |
| `503 ... did not answer within 3s` | le moteur ne répond plus du tout |

**Ce que fait l'orchestrateur d'un healthcheck rouge** dépend de lui, et il
faut le savoir avant de compter dessus :

- `docker run` / `docker compose` seuls : un conteneur `unhealthy` **n'est
  pas redémarré**. Le healthcheck est une alerte (`docker ps` le montre,
  Prometheus le scrape) ; le redémarrage automatique ne se déclenche que
  quand le process meurt réellement.
- Docker Swarm, Kubernetes, Nomad : le conteneur est tué et recréé après
  `retries` échecs consécutifs. Avec `--interval=30s --retries=3`, cela
  fait ~90 s d'échecs de suite — assez pour ne pas réagir à un hoquet.
- En Kubernetes, mapper `/health` sur la *liveness* probe (redémarrage) et
  la *readiness* probe (retrait du service) donne les deux comportements.

Pour forcer un redémarrage sur healthcheck rouge sans orchestrateur,
[`autoheal`](https://github.com/willfarrell/docker-autoheal) fait
exactement ça et rien d'autre.

### Métriques

`GET /metrics` sur le même port, au format d'exposition Prometheus :
connexions (total/actives/refusées), échecs d'authentification, requêtes
par type, latence (histogramme), erreurs par catégorie `FerriteError`,
transactions actives/committées/annulées, taille du fichier de données et
du journal, progression de l'id de transaction contre son plafond.

Le port **n'est pas authentifié** (comme tout endpoint Prometheus) : le
`docker-compose.yml` ne le publie volontairement pas sur l'hôte, un
Prometheus du même réseau Docker scrape `ferrite:9187`.

### Variables d'environnement

| Variable | Défaut | Rôle |
| --- | --- | --- |
| `FERRITE_LISTEN` | `0.0.0.0:5432` | adresse d'écoute SQL |
| `FERRITE_DATA` | `./data` (`/data` dans l'image) | répertoire de `ferrite.db` et `ferrite.wal` |
| `FERRITE_USER` | `ferrite` | compte unique du bootstrap |
| `FERRITE_PASSWORD` | généré aléatoirement | **à définir en production** |
| `FERRITE_TLS_CERT` / `FERRITE_TLS_KEY` | — | chaîne PEM + clé privée ; les deux ou aucun |
| `FERRITE_TLS_DISABLE` | non | `1` pour accepter du clair, avec un avertissement |
| `FERRITE_AUTH_MAX_FAILURES` | `5` | échecs d'auth tolérés par fenêtre, par IP et par nom d'utilisateur |
| `FERRITE_AUTH_WINDOW` | `60` | largeur de la fenêtre glissante, en secondes |
| `FERRITE_AUTH_LOCKOUT` | `300` | durée du verrouillage temporaire, en secondes |
| `FERRITE_AUTH_THROTTLE_DISABLE` | non | `1` pour ne pas limiter du tout, avec un avertissement |
| `FERRITE_METRICS_LISTEN` | `0.0.0.0:9187` | endpoint `/metrics` + `/health` |
| `FERRITE_METRICS_DISABLE` | non | `1` pour ne pas ouvrir cet endpoint (et donc perdre le healthcheck) |
| `FERRITE_HEALTH_URL` | `http://127.0.0.1:9187/health` | ce qu'interroge le `HEALTHCHECK` de l'image ; à changer avec `FERRITE_METRICS_LISTEN` |
| `FERRITE_LOG` | `info` | filtre `tracing-subscriber` |

### Ce qui est vérifié

`crates/ferrite-server/tests/restart.rs` tue le serveur en pleine charge
d'écriture (4 clients concurrents, `SIGKILL`, donc aucun checkpoint) puis
le relance avec exactement la même ligne de commande, sans aucune étape
manuelle entre les deux : chaque insertion dont le client avait reçu l'`OK`
est retrouvée, le serveur reprend les écritures, et `/health` repasse au
vert par la seule récupération par le journal.
