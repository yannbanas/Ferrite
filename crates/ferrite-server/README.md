# ferrite-server

Le binaire. Ecoute sur le port **5432** et sert le protocole de fil
PostgreSQL via `ferrite-protocol`.

## Etat du cablage — a lire en premier

Le serveur sert aujourd'hui `ferrite_protocol::mock::MockHandler`, pas un
vrai moteur.

Tout ce qui est **sous** le trait `QueryHandler` est reel et teste : TLS,
authentification, framing, cycle simple, cycle etendu, gestion d'erreur.
Tout ce qui est **au-dessus** est un bouchon : le mock repond a une
poignee d'instructions cablees en dur (`SELECT 1`, `SELECT * FROM pets`,
`BEGIN`/`COMMIT`, `INSERT ...`) et rejette le reste.

La raison est deliberee : `ferrite-storage`, `ferrite-catalog`,
`ferrite-sql`, `ferrite-planner`, `ferrite-exec` et `ferrite-proc` etaient
encore des scaffolds au moment de l'ecriture. Brancher un moteur inexistant
aurait donne un cablage bancal impossible a tester ; le bouchon donne une
frontiere nette.

**Pour brancher le vrai moteur** : une seule fonction change,
`build_handler()` dans `src/main.rs`. Elle doit rendre un
`Arc<dyn QueryHandler>` construit sur `ferrite-exec`, et les dependances
moteur doivent revenir dans `Cargo.toml` (elles en ont ete retirees pour ne
pas coupler la compilation du binaire a du travail en cours).

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
| `FERRITE_USER` | `ferrite` | compte unique du bootstrap |
| `FERRITE_PASSWORD` | genere aleatoirement | mot de passe de ce compte |
| `FERRITE_TLS_CERT` / `FERRITE_TLS_KEY` | — | chaine PEM + cle privee ; les deux ou aucun |
| `FERRITE_TLS_DISABLE` | non | `1` pour accepter du clair, **avec un avertissement** |
| `FERRITE_LOG` | `info` | filtre `tracing-subscriber` |

TLS est actif par defaut. `FERRITE_TLS_DISABLE` est une sortie de secours
pour du loopback ou un transport deja securise, pas un mode normal.

Le compte unique est provisoire : il disparait des que `ferrite-catalog`
expose une table de roles et qu'un `Authenticator` peut la lire.

## Tests

```bash
cargo test -p ferrite-server --all-targets
```

`tests/boot.rs` lance le vrai binaire en processus fils et lui parle avec
`tokio-postgres` : il verifie qu'une requete aboutit sur un listener en
clair, et qu'un listener par defaut refuse une session non chiffree.
