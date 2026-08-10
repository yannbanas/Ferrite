# ferrite-server

Le binaire. Ecoute sur le port **5432** et sert le protocole de fil
PostgreSQL via `ferrite-protocol`, adosse au vrai moteur.

## Ce que `build_handler` assemble

```text
ferrite-storage   FerriteStorage::open(FERRITE_DATA)   pages, B+-tree, MVCC, journal
ferrite-catalog   SystemCatalog (bootstrap | open)     schemas + index, stockes en tables
ferrite-sql       parse()                              texte -> AST
ferrite-planner   Planner                              AST -> plan physique
ferrite-exec      Session                              execution, declenche les triggers
ferrite-proc      ProcRegistry                         roles, permissions, triggers
```

Le tout est construit **une fois** au demarrage. `src/engine.rs` expose
`Engine` (la moitie partagee) et `Connection` (une session cliente).
`QueryHandler::connect` donne a chaque connexion sa propre `Connection`,
seul endroit ou vit un etat de session : la transaction ouverte par
`BEGIN`.

Une instruction part dans une des trois directions :

| | |
| --- | --- |
| `BEGIN`/`COMMIT`/`ROLLBACK` | gere dans `engine.rs` : c'est l'etat de session |
| `CREATE`/`DROP TABLE`, `CREATE`/`DROP INDEX` | va directement a `ferrite-catalog` (`*_in`, donc dans la transaction du client) |
| tout le reste | `ferrite-planner` puis `ferrite-exec` |

Hors transaction explicite, chaque instruction ouvre et valide la sienne —
l'autocommit de PostgreSQL. Dans une transaction, le snapshot est repris a
chaque instruction (`read committed`), sans quoi une transaction ouverte
avant un `CREATE TABLE` valide ailleurs ne verrait jamais la table.

Le stockage est synchrone et prend un verrou global : chaque instruction
part sur `tokio::task::spawn_blocking`, pour qu'un scan long ne bloque pas
un worker dont les autres connexions ont besoin.

## Identite et permissions

Le compte de bootstrap a **une seule** identite
(`identity_for_user(FERRITE_USER)`), utilisee des deux cotes du modele de
securite : `StaticAuthenticator` la delivre a la connexion, et
`ProcRegistry::grant_role` est ce qui lui donne effectivement des droits.
Sans ce grant, la connexion reussirait et chaque instruction serait
refusee — le modele est deny-by-default et l'authentification reseau seule
ne confere rien.

## Lancer

```bash
cargo run -p ferrite-server
```

Sans configuration, le serveur genere un certificat auto-signe ephemere et
un mot de passe aleatoire, tous deux journalises au demarrage. Un client se
connecte alors avec `sslmode=require` (pas `verify-full` : le certificat
n'est signe par personne).

## Configuration

| Variable | Defaut | Role |
| --- | --- | --- |
| `FERRITE_LISTEN` | `0.0.0.0:5432` | adresse d'ecoute |
| `FERRITE_DATA` | `./data` | repertoire de `ferrite.db` et `ferrite.wal` |
| `FERRITE_USER` | `ferrite` | compte unique du bootstrap |
| `FERRITE_PASSWORD` | genere aleatoirement | mot de passe de ce compte |
| `FERRITE_TLS_CERT` / `FERRITE_TLS_KEY` | — | chaine PEM + cle privee ; les deux ou aucun |
| `FERRITE_TLS_DISABLE` | non | `1` pour accepter du clair, **avec un avertissement** |
| `FERRITE_AUTH_MAX_FAILURES` | `5` | echecs d'auth toleres par fenetre |
| `FERRITE_AUTH_WINDOW` | `60` | fenetre glissante, en secondes |
| `FERRITE_AUTH_LOCKOUT` | `300` | verrouillage temporaire, en secondes |
| `FERRITE_AUTH_THROTTLE_DISABLE` | non | `1` pour ne pas limiter, **avec un avertissement** |
| `FERRITE_METRICS_LISTEN` | `0.0.0.0:9187` | endpoint `/metrics` + `/health` |
| `FERRITE_METRICS_DISABLE` | non | `1` pour ne pas ouvrir cet endpoint |
| `FERRITE_LOG` | `info` | filtre `tracing-subscriber` |

## Observabilite

Un second listener, HTTP en clair, separe du port PostgreSQL :

- `GET /metrics` — format d'exposition Prometheus (voir `ferrite-metrics`).
- `GET /health` — un vrai aller-retour moteur : le catalogue est lu, une
  transaction est ouverte et validee (donc le journal est ecrit et
  `fsync`e). Rend `200` si le moteur repond, `503` avec la raison sinon,
  et `503` sur echeance de 3 s s'il ne repond plus du tout. C'est ce que
  le `HEALTHCHECK` de l'image interroge : `nc -z 5432` ne prouvait que
  l'existence du listener, pas que le moteur repondait encore.

Une sonde qui ne revient pas n'est pas suivie d'une autre — la suivante
constate que la precedente est toujours en vol et rend `503`
immediatement, au lieu d'empiler des threads bloquants derriere le meme
verrou.

L'echec de binding de cet endpoint est fatal au demarrage : un
deploiement qui l'a demande et ne l'a pas obtenu n'a pas de healthcheck
non plus.

TLS est actif par defaut. `FERRITE_TLS_DISABLE` est une sortie de secours
pour du loopback ou un transport deja securise, pas un mode normal.

Le compte unique est provisoire : il disparait des que `ferrite-catalog`
expose une table de roles et qu'un `Authenticator` peut la lire.

## Limites connues du cablage

- `SELECT` sans `FROM` (`SELECT 1`, `SELECT version()`) n'existe pas : le
  planificateur part toujours d'une table.
- Pas de `RETURNING`, pas de `ORDER BY`, pas d'agregats, pas de `JOIN`,
  pas de sous-requetes, pas d'`ALTER TABLE` — voir `ferrite-planner`.
- `CREATE PROCEDURE`/`CREATE TRIGGER` sont refuses : les procedures sont
  des closures Rust enregistrees au demarrage (`ferrite-proc`), il n'y a
  pas de langage procedural en v1.
- Une chaine multi-instructions s'execute en entier ; seul le resultat de
  la derniere revient au client.

## Tests

```bash
cargo test -p ferrite-server --all-targets
```

`tests/boot.rs` lance le vrai binaire en processus fils et lui parle avec
`tokio-postgres` : TLS exige par defaut, cycle complet DDL/DML avec index
et parametres lies, transaction annulee, survie a un redemarrage apres un
`kill` (donc recuperation par le journal), conflit d'ecriture entre deux
connexions rendu en `40001`, et refus propre de tout ce que le sous-ensemble
v1 ne couvre pas sans casser la connexion.
