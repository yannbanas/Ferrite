# Ferrite — plan d'architecture et décisions prises

Ce document capture toutes les décisions de conception prises avant le début de l'implémentation, pour pouvoir reprendre le projet plus tard sans avoir à retrouver le fil. Il ne décrit pas l'état actuel du code (voir le code + les rapports d'agents pour ça), mais l'intention de départ.

## Objectif

Un clone de PostgreSQL en 100 % Rust, plus léger et plus rapide, en coupant délibérément les fonctionnalités à faible usage/forte complexité plutôt qu'en visant la parité complète.

## Choix d'architecture

- **Protocole de fil compatible PostgreSQL** (protocole v3, port **5432**) — psql, drivers JDBC/ODBC, sqlx, Diesel, tout l'écosystème client existant doit fonctionner sans modification. C'est le levier de compatibilité le plus rentable : on ne réimplémente jamais l'écosystème client.
- **Sous-ensemble de SQL**, pas la grammaire complète (voir liste des coupes plus bas).
- **Moteur de stockage écrit de zéro** (format de page, B-tree, versions de lignes MVCC) — pas de moteur clé-valeur existant (`redb`/`sled`) avec du SQL plaqué dessus.
- **MVCC par snapshot isolation** (xmin/xmax façon Postgres), `TxnId` en `u64` — pas de gestion du wraparound d'ID de transaction en v1 (non pertinent avec un espace 64 bits à court/moyen terme).
- **Mono-nœud** pour la v1, aucune réplication.

## Modèle de sécurité — décision importante

**Pas de langage de policy déclaratif façon `CREATE POLICY ... USING (...)`.** À la place : les triggers et procédures stockées (conservés, voir plus bas) reçoivent l'identité de l'appelant (`ferrite_common::Identity`, façon `SpacetimeDB::Identity`) et décident de l'accès en code — le même principe qu'un reducer SpacetimeDB qui lit `ctx.sender()`. Un seul moteur procédural à construire au lieu de deux (RLS + procédures séparément), et un modèle plus flexible qu'un DSL de policy.

Rôles : `ferrite_common::Role` — un nom + une liste plate de `Permission` (Connect, CreateTable, Select, Insert, Update, Delete, Execute, Admin). Pas de grants par colonne, pas de chaînes `WITH GRANT OPTION`.

## Ce qu'on garde (contrairement à la coupe initiale envisagée)

- **Triggers et procédures stockées** — gardés, sous-ensemble minimal au départ. C'est aussi le point d'ancrage du modèle de sécurité (voir ci-dessus), donc structurellement important, pas juste une fonctionnalité de confort.

## Ce qu'on coupe pour rester léger (v1)

- Système d'extensions (`CREATE EXTENSION`, PL/pgSQL, PL/Python…).
- Recherche plein texte (tsvector/tsquery).
- Partitionnement de tables, héritage.
- Foreign data wrappers.
- Types d'index avancés — **B-tree seul** en v1 (pas de GiST/GIN/BRIN/hash).
- Réplication logique/streaming, PITR, archivage WAL — reporté entièrement.
- Savepoints (transactions imbriquées) — v2.
- LISTEN/NOTIFY.
- Types avancés (arrays, ranges, types géométriques, domaines custom) — types scalaires + JSON seulement (voir `ferrite_common::DataType`).
- Exécution parallèle de requêtes — exécuteur mono-thread en v1.
- **Optimiseur basé sur les coûts** — remplacé par un planificateur à règles fixes (predicate pushdown, heuristique simple index-vs-scan), pas de modèle statistique.
- Autovacuum classique — stratégie de compaction/MVCC plus simple à définir dans `ferrite-storage`.

## Exigences transverses (sécurité / fiabilité / moderne)

- TLS activé par défaut sur le protocole de fil (`ferrite-protocol`, `tokio-rustls`) — pas un flag optionnel.
- Comparaison en temps constant pour tout ce qui touche l'auth (crate `subtle`, même convention que `pawchat-kv`/`chronotope-server`).
- Modèle de permissions deny-by-default.
- Checksums de page activés par défaut dans `ferrite-storage`.
- Fuzzing du parseur SQL (`ferrite-sql`) et du parseur de protocole (`ferrite-protocol`) en CI (`cargo-fuzz`).
- Tests par propriétés (`proptest`) sur les invariants MVCC (`ferrite-storage`) — les violations de sérialisabilité sont le genre de bug qu'un test unitaire classique ne voit pas.
- JSON natif, UUID v7 par défaut.
- Tout en async (`tokio`).
- Métriques exposées façon Prometheus (à ajouter, pas encore scaffoldé — voir « Reste à faire » plus bas).
- Journalisation structurée (`tracing`) sur tout événement sensible (échec d'auth, refus de permission, changement de schéma).

## Structure du workspace

```
Ferrite/
  Cargo.toml                    workspace, [workspace.package], [workspace.dependencies]
  crates/
    ferrite-common/             types + traits partagés — AUCUNE implémentation, contrat only
    ferrite-storage/            Agent 1 — pages, B-tree, MVCC, journal de récupération
    ferrite-catalog/            Agent 2 — catalogue système, sur StorageEngine
    ferrite-sql/                Agent 2 — lexer/parser -> AST
    ferrite-planner/            Agent 3 — AST -> plan logique -> plan physique (à règles)
    ferrite-exec/                Agent 3 — exécuteur, appelle storage/catalog/proc
    ferrite-proc/                Agent 3 — triggers, procédures stockées, sécurité par identité
    ferrite-protocol/           Agent 4 — protocole PostgreSQL v3, TLS, auth
    ferrite-server/             Agent 4 — binaire, port 5432
```

### Contrat partagé (`ferrite-common`)

- `Value`/`DataType` — types scalaires (Boolean, Int4, Int8, Float8, Text, Timestamp, Uuid, Json).
- `Row` — valeurs positionnelles, pas d'identité de ligne (l'identité/version MVCC vit dans `ferrite-storage`).
- `Schema`/`ColumnDef`/`TableId`.
- `Identity`/`Role`/`Permission` — modèle de sécurité, voir plus haut.
- `TxnId`/`Snapshot` — MVCC, `u64`, pas de wraparound géré en v1.
- `FerriteError` — un seul type d'erreur partagé par tout le workspace (`thiserror`).
- `trait StorageEngine` — begin/commit/abort, snapshot, insert/update/delete/get/scan, create/drop table.
- `trait Catalog` — table_id, table_schema, create/drop/list tables.

Ces deux traits sont volontairement un contrat v0 : les agents peuvent proposer des changements, mais toute évolution doit être coordonnée ici (`ferrite-common`) puisque tout le reste du workspace en dépend.

## Découpage en agents

**Agent 1 — `ferrite-storage`** : pages, B-tree, versions MVCC, journal de récupération après crash, checksums de page.

**Agent 2 — `ferrite-sql` + `ferrite-catalog`** : lexer/parser SQL (sous-ensemble v1) -> AST, et catalogue système construit sur `StorageEngine`.

**Agent 3 — `ferrite-planner` + `ferrite-exec` + `ferrite-proc`** : plan logique/physique à règles, exécuteur mono-thread, moteur de triggers/procédures stockées (et donc le point d'ancrage de la sécurité par identité).

**Agent 4 — `ferrite-protocol` + `ferrite-server`** : protocole PostgreSQL v3 (TLS par défaut, auth), binaire qui assemble tout et écoute sur 5432.

Ordre de dépendance : Storage → (Catalog, SQL) → (Planner, Exec, Proc) → (Protocol, Server). Le scaffold (ce commit) fournit des stubs qui compilent pour les 4 domaines, donc les agents peuvent démarrer en parallèle contre un contrat stable plutôt que de se marcher dessus.

## Infrastructure

Même patron que `chronotopedb`/`pawchat-kv` (dépôts sœurs du même auteur) : Dockerfile multi-stage `cargo-chef` + cible musl statique + image `alpine` finale, CI GitHub Actions (`fmt`, `clippy -D warnings`, `test` + `test --doc`, `cargo-audit --deny warnings`), publication `ghcr.io/mairie-creusot/ferrite` sur push vers `main`, package GHCR privé (pas de raison de le rendre public pour un projet à ce stade).

## Reste à faire (pas encore scaffoldé, à trancher plus tard)

- Endpoint de métriques Prometheus — pas encore de crate/emplacement défini.
- `cargo-fuzz` : cibles à écrire une fois `ferrite-sql`/`ferrite-protocol` non-triviaux.
- Format exact du journal de récupération de `ferrite-storage` (WAL complet vs plus simple) — laissé au jugement de l'Agent 1 avec justification.
- Sous-ensemble exact de la grammaire SQL v1 (JOIN, CTE, quelles fonctions d'agrégat, quelles fenêtres) — laissé au jugement de l'Agent 2 avec justification, dans les limites du présent document.
