# ferrite-protocol

Le protocole de fil PostgreSQL, version 3, cote serveur.

C'est le levier de compatibilite de Ferrite : psql, JDBC/ODBC, `sqlx`,
`tokio-postgres` et Diesel fonctionnent sans modification contre un serveur
qui produit les bons octets. Aucun bout de l'ecosysteme client n'est
reimplemente.

## Ce qui est supporte

| Domaine | Etat |
| --- | --- |
| `StartupMessage`, `SSLRequest`, `GSSENCRequest`, `CancelRequest` | oui (GSSAPI refuse par `N`, cancel accuse par fermeture) |
| Authentification | `AuthenticationCleartextPassword` sur TLS |
| Cycle simple (`Query` -> `RowDescription`/`DataRow`/`CommandComplete`) | oui |
| Cycle etendu (`Parse`/`Bind`/`Describe`/`Execute`/`Close`/`Sync`/`Flush`) | oui, formats texte **et** binaire, `Execute` a nombre de lignes limite avec `PortalSuspended` |
| Etat transactionnel dans `ReadyForQuery` | oui (`I`/`T`/`E`) |
| `ErrorResponse` avec SQLSTATE | oui, un code par variante de `FerriteError` |
| TLS | **active par defaut**, `tokio-rustls` + *ring* |

Types couverts, avec les OID PostgreSQL reels, en texte et en binaire :
`bool`, `int4`, `int8`, `float8`, `text`, `timestamptz`, `uuid`, `json`.

## Ce qui n'est pas supporte

- **SCRAM-SHA-256** (RFC 7677). Le mot de passe en clair sur TLS est le
  mecanisme v1 ; SCRAM est une amelioration ciblee, pas un blocage — la
  plomberie de messages (`AuthenticationSASL`) est un simple message `R`.
  Voir les docs du module `auth`.
- **Annulation de requete** (`CancelRequest` est accuse puis la connexion
  est fermee, aucune requete n'est reellement interrompue).
- **`COPY`**, `FunctionCall`, `LISTEN`/`NOTIFY` : ces types de message sont
  refuses proprement, sans perte de synchronisation.
- **Requetes multi-instructions** dans un seul `Query` : la chaine entiere
  est passee au `QueryHandler`. Decouper sur `;` demande le lexer SQL, qui
  vit dans `ferrite-sql`.
- Protocole version 2 : refuse explicitement.

## Point d'integration : `QueryHandler`

Ce crate ne depend **que** de `ferrite-common`. Il ne connait ni le moteur
de stockage, ni le planificateur, ni l'executeur. Tout passe par un trait :

```rust
#[async_trait]
pub trait QueryHandler: Send + Sync + 'static {
    async fn execute(&self, sql: &str, caller: Identity) -> Result<QueryResult, FerriteError>;

    async fn execute_params(&self, sql: &str, params: &[Value], caller: Identity)
        -> Result<QueryResult, FerriteError> { /* delegue a execute */ }

    async fn describe(&self, sql: &str, caller: Identity)
        -> Result<StatementDescription, FerriteError> { /* StatementDescription::unknown() */ }
}
```

`execute` suffit pour le cycle simple. `execute_params` et `describe` ont
une implementation par defaut ; un moteur qui veut servir les drivers a
requetes preparees (tokio-postgres, sqlx, JDBC) doit surcharger les deux.

L'`Identity` authentifiee est transmise a chaque appel : c'est ce qui
alimente le modele de securite par code de `ferrite-proc` (pas de langage de
policy declaratif, voir `docs/architecture.md`).

`mock::MockHandler` implemente ce trait avec un jeu d'instructions cable en
dur. Il existe pour prouver le protocole de bout en bout sans moteur, et
c'est ce que sert `ferrite-server` tant que `ferrite-exec` n'est pas pret.

## Securite

- **TLS par defaut** : `TlsMode::Disabled` doit etre nomme explicitement.
  Un `StartupMessage` en clair sur un listener TLS est refuse par un
  `ErrorResponse` FATAL **avant** meme qu'un mot de passe soit demande.
- **Comparaison en temps constant** (`subtle`) sur des condensats sales de
  taille fixe : ni le contenu ni la longueur du mot de passe ne fuient. Un
  utilisateur inconnu coute le meme temps qu'un mot de passe faux.
- **Deny-by-default** : sans `Permission::Connect` (ou `Admin`), pas de
  session.
- **Decodage total** : longueurs invalides, comptes negatifs, chaines non
  terminees, texte non-UTF-8, octets residuels et troncatures produisent une
  `ProtocolError`, jamais un panic. La taille de trame est verifiee **avant**
  toute allocation.
- Journalisation `tracing` sur chaque echec d'auth et chaque refus de
  connexion.

## Tests

```bash
cargo test -p ferrite-protocol --all-targets
```

- `tests/wire.rs` — vraies connexions TCP vers un vrai listener : login,
  cycle simple, cycle etendu, portails suspendus, erreurs, concurrence.
- `tests/tls.rs` — negociation TLS, refus du clair, refus d'un certificat
  non approuve.
- `tests/external_client.rs` — **`tokio-postgres`**, un client PostgreSQL
  independant, en clair et en TLS, en formats binaires. `psql` n'etait pas
  disponible dans l'environnement de developpement.
- `tests/malformed.rs` — trames tronquees, longueurs hostiles, flux
  aleatoire : la propriete testee est l'absence de panic.

### Fuzzing

```bash
cargo +nightly fuzz run message_decode
cargo +nightly fuzz run session
```

`fuzz/` est un workspace separe (cargo-fuzz demande nightly et libFuzzer).
`message_decode` cible les decodeurs de messages, `session` cible la machine
a etats complete. Les deux tournent en CI (`.github/workflows/fuzz.yml`) :
60 s par PR, 15 min chaque nuit.
