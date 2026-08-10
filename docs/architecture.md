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

## Ce que coûte un vrai schéma applicatif (mesuré, août 2026)

Le schéma SQLite de PawChat (72 tables, 672 colonnes, 938 lignes réelles) a
été traduit dans le sous-ensemble Ferrite (`tools/sqlite_to_ferrite.py`) puis
rejoué contre un vrai serveur (`crates/ferrite-server/tests/replay.rs`) :
**72/72 tables créées, 938/938 lignes chargées et relues**. Le DDL et les
données passent. Ce qui bloque est ailleurs, et voici l'ordre de priorité que
ce test fait ressortir :

1. ~~**`ALTER TABLE`**~~ — *fait.* PawChat porte 195 `ALTER TABLE ... ADD
   COLUMN` sur 22 tables ; c'est sa mécanique de migration. Sans ça, une
   application ne peut pas évoluer sur Ferrite, seulement démarrer.
2. ~~**`DEFAULT` appliqué à l'insertion**~~ — *fait.* La grammaire le parsait
   et le planificateur l'ignorait. Une colonne omise recevait `NULL`, ce qui
   échouait sur `NOT NULL` (visible) mais passait silencieusement sur une
   colonne nullable (invisible). C'était le plus dangereux des manques.
3. ~~**`JOIN`**~~ — *fait.* Un schéma relationnel normalisé n'est lisible qu'à
   travers des jointures.
4. ~~**Agrégats (`count`/`sum`/…) et `ORDER BY`**~~ — *faits*, avec
   `GROUP BY`/`HAVING`, `DISTINCT` et `LIKE`.
5. ~~**`CASE`, `CAST`, fonctions scalaires, `ILIKE`, `COLLATE NOCASE`,
   idiomes d'upsert, `IN (SELECT ...)` non corrélé**~~ — *faits*, sur la
   base d'un audit de tout le SQL réellement émis par PawChat
   (1287 littéraux, 181 fichiers) : voir `docs/pawchat-sql-audit.md`.
6. **Sous-requête scalaire corrélée, `EXISTS`, `UNION`, `SELECT` sans
   `FROM`, index partiels** — ensuite. Ce sont les 13 derniers littéraux
   PawChat que Ferrite refuse, chacun en le nommant.

Les contraintes que la traduction doit encore jeter faute d'équivalent :
4 `FOREIGN KEY`, 13 `UNIQUE`, 1 `COLLATE`, 47 `AUTOINCREMENT`. Aucun `CHECK`,
aucun trigger, aucune vue dans ce schéma. 6 des 7 index se créent ; le
septième est partiel (`WHERE status = 'paid'`).

### Après `ALTER TABLE` + `DEFAULT` (même mesure, même base)

Le traducteur émet maintenant les `DEFAULT` qu'il jetait (200 des 239 ; les
39 restants sont des `datetime('now')` sur des colonnes que les valeurs ne
permettent pas de typer `TIMESTAMP`, et sont refusés plutôt qu'ignorés). Le
fichier `_after.sql` rejoue en plus, sur les 72 tables déjà remplies, les
deux formes de migration qu'une application utilise vraiment — une colonne
nullable et une colonne `NOT NULL DEFAULT` —, la ré-exécution de la même
migration (`IF NOT EXISTS`, ce que fait un `try/catch` côté PawChat), une
relecture des lignes écrites avant ces colonnes, et un `INSERT` ne nommant
que les colonnes que l'application n'a pas le choix de fournir :

```
avant : 72/72 tables, 938/938 lignes,   0/313 énoncés `_after` acceptés
après : 72/72 tables, 938/938 lignes, 313/313 énoncés `_after` acceptés
```

Les 313 refus d'avant : 216 `parse error: found identifier alter`, 72
`column not found: ferrite_added`, et 25 `INSERT` tombant sur un
`... is not nullable` — dont exactement celui du démarrage de PawChat,
`INSERT INTO users (id, username, password)` refusé sur
`users.created_at is not nullable`.

Une ligne écrite avant une colonne n'est pas réécrite : `ADD COLUMN` reste
une écriture `O(1)` dans le catalogue, et c'est la lecture qui réconcilie
l'arité en complétant la ligne avec le `DEFAULT` constant de la colonne (ou
`NULL`). C'est le choix de PostgreSQL (`pg_attribute.attmissingval`), et la
raison pour laquelle `ADD COLUMN NOT NULL` sans défaut constant est refusé
sur une table non vide plutôt que de publier un `NULL` dans une colonne
déclarée sans.

### Les deux passes ensemble (même mesure, même base)

`_after.sql` enchaîne maintenant les deux jeux dans une seule exécution : le
DDL d'index, les requêtes applicatives, les 313 énoncés de migration, puis
**les mêmes requêtes une seconde fois**, contre des tables qui viennent de
gagner deux colonnes. C'est le seul endroit où les deux fonctionnalités se
croisent pour de vrai — une ligne écrite avant l'`ADD COLUMN` arrive dans la
jointure, le tri et l'agrégat plus courte que son schéma si le scan ne
réconcilie pas l'arité d'abord.

```
72/72 tables, 938/938 lignes, 337/337 énoncés `_after` acceptés
```

Après l'application réelle des contraintes d'unicité, le même replay rend
`433/435` (les deux refus restants étant le `SELECT` sans `FROM`, joué deux
fois), et trois énoncés y sont désormais *attendus en échec* : réinsérer la
ligne `users` de PawChat rend un `23505` et le `count(*)` reste à 22 de part
et d'autre. Le probe de migration, lui, recopiait la clé primaire d'une
ligne existante — un doublon depuis toujours, que seule l'absence de
contrainte rendait acceptable ; il génère maintenant une clé fraîche.

`count(*)` sur `vr_room_objects` rend 394 avant les migrations et 395 après,
la ligne de plus étant l'`INSERT` qui ne nomme que les colonnes obligatoires
et laisse le reste aux `DEFAULT`. La même bascule se lit sur le
`LEFT JOIN ... GROUP BY ... HAVING` de `vr_room_members` (2 groupes puis 3).

Une limite connue, sans rapport avec les migrations : un scope atteint une
relation par son nom *et* par son alias, donc `FROM t JOIN t t2` laisse `t.a`
désigner deux colonnes et l'auto-jointure est refusée comme ambiguë. En
PostgreSQL l'alias masque le nom de la table.

## Extensions syntaxiques envisagées (après la priorité PawChat, pas avant)

Proposition de l'utilisateur (août 2026) : au-delà de la couverture SQL standard, un jeu de mots-clés qui n'existent dans aucune base grand public, pensés pour éliminer l'imbrication et exploiter ce que Ferrite a déjà (MVCC, identité, modèle procédural). Séquencé explicitement **après** que `ALTER TABLE`/`DEFAULT`/`JOIN`/agrégats fassent tourner PawChat pour de vrai — ce n'est pas la priorité actuelle, c'est la suite. Tri par faisabilité réelle, pas par ordre de préférence :

**Contrainte transverse, non négociable** : aucun de ces mots ne doit être réservé. `user`, `session`, `live`, `why` sont des noms de colonne plausibles dans un vrai schéma (PawChat en a plusieurs). Postgres résout ça avec des mots-clés *contextuels* — reconnus seulement à la position grammaticale où ils ont un sens, jamais en conflit avec un identifiant. `ferrite-sql` doit suivre le même principe pour chaque mot-clé ajouté ci-dessous, sans exception.

**Investissement à faire en premier, avant les mots-clés eux-mêmes** : la syntaxe pipeline `|>` (Google, *"SQL Has Problems. We Can Fix Them"*, VLDB 2024 ; PRQL en a fait tout son projet) — `FROM t |> WHERE ... |> AGGREGATE ...` au lieu de l'imbrication classique. Ça change la forme de l'AST de façon à rendre plusieurs mots-clés ci-dessous optionnels (leur intérêt principal était justement de tuer l'imbrication) plutôt que de les traiter un par un. À évaluer avant de committer sur la liste complète : peut-être qu'une partie de la Famille A devient inutile une fois `|>` en place.

### Faisable à court/moyen terme (sucre syntaxique ou builtin borné)

- **`TOP n PER x ORDER BY ...`** — top-N par groupe, généralisation du `DISTINCT ON` déjà présent en Postgres (cas dégénéré n=1). Se désucre vers une passe partition+tri+limite-par-partition ; pas besoin de fonctions de fenêtrage génériques pour livrer *ce* mot-clé spécifiquement, seulement pour l'opérateur qu'il implique.
- **`SESSIONIZE BY ... GAP ...`** — gaps-and-islands. Opérateur spécialisé de la même famille que PER (partition + tri + rupture sur un seuil).
- **`DECAY(half_life => ...)` / `FORGET FROM ... WHERE weight < ...`** — poids qui décroît avec le temps (`exp(-Δt/τ)`) comme fonction native utilisable dans `ORDER BY`/`RANK BY`, et `FORGET` comme sucre pour un `DELETE` sur ce poids calculé. Pas de nouvel opérateur d'exécution, juste une fonction builtin + réutilisation de `DELETE`.
- **`UNIT`** — typage dimensionnel sur les colonnes numériques (`NUMERIC UNIT meters`), vérifié/inféré à travers les expressions arithmétiques au moment du plan. Borné : une passe de vérification de type en plus, pas un changement de moteur d'exécution.
- **`INSERT ... ON CONFLICT DO UPDATE/NOTHING`** (ajout hors liste utilisateur, standard Postgres) — déjà noté comme non couvert par `ferrite-sql`. Pas exotique, mais c'est l'opération la plus demandée en pratique (upsert) et absente de la liste ci-dessus ; à prioriser au même niveau que PER en termes de valeur réelle.
- **`QUALIFY`** (ajout, façon Snowflake/DuckDB) — filtrer sur le résultat d'une fonction de fenêtre sans sous-requête. Même esprit que PER, généralise au cas où la fonction de fenêtre n'est pas un simple top-N.

### Faisable mais plus gros (touche le moteur de stockage, pas juste le parseur/planificateur)

- **`AS OF` (point-in-time)** — Ferrite garde déjà les versions MVCC d'une ligne dans une seule charge utile B-tree, mais avec élagage local dès qu'aucun snapshot actif n'en a besoin (voir `ferrite-storage/README.md`) : pas de rétention longue durée par défaut. `AS OF` demande une vraie politique de rétention configurable (garder N jours d'historique avant élagage) — c'est un changement de politique de stockage, pas juste un mot-clé. Prior art sérieux : SQL:2011 (system-versioned tables), Datomic, XTDB.
- **`DIFF ... BETWEEN`** — le « git diff » des données, dérivé d'`AS OF` une fois la rétention en place. Rare en pratique (pas seulement dans les bases grand public) ; vaut le coup précisément parce que c'est rare.
- **`TRAVERSE ... DEPTH n..m ACYCLIC`** — remplace `WITH RECURSIVE` pour les traversées de graphe. Standardisé récemment (SQL:2023 SQL/PGQ, `GRAPH_TABLE`) donc pas une invention isolée, mais demande une vraie stratégie d'exécution itérative avec détection de cycle dans `ferrite-exec`, distincte du reste du moteur à règles.
- **`SHAPE` / projection imbriquée façon GraphQL** (`SELECT user { name, posts: { title } }`) — nécessite que le catalogue connaisse les relations de clé étrangère pour dérivation automatique des jointures. Bloqué tant que les `FOREIGN KEY` restent jetées à l'import (voir plus haut, 4 FK jetées sur le schéma PawChat) : ce mot-clé a une vraie dépendance amont à poser avant lui — stocker et exposer les FK dans `ferrite-catalog`.

### Recherche, honnêtement — pas un chantier de sprint

- **`LIVE SELECT` / `MAINTAIN VIEW`** — flux de deltas en continu / vue matérialisée à maintenance incrémentale (differential dataflow, façon Materialize `SUBSCRIBE`). C'est un moteur de calcul incrémental entier, pas une fonctionnalité qu'on ajoute à un exécuteur à plans matérialisants comme celui de `ferrite-exec` v1 aujourd'hui. Intéressant précisément parce que Ferrite a déjà un modèle d'identité proche de SpacetimeDB (souscriptions par salle) — mais soyons clairs : ça se compte en mois d'ingénierie de recherche, pas en semaines, et ça suppose probablement de revoir l'exécuteur pour du dataflow incrémental plutôt que du pull matérialisant.
- **`EXPLAIN WHY` (provenance)** — chaque tuple résultat porterait un polynôme de provenance (Green, Karvounarakis & Tannen, PODS 2007) au lieu d'un plan d'exécution classique. Fondement théorique solide, mais jamais industrialisé dans une base grand public pour une bonne raison : ça demande d'instrumenter chaque opérateur pour propager la provenance, pas juste d'ajouter une commande.
- **`CONFIDENCE` / `MAYBE` (bases probabilistes)** — étendre le modèle de valeur pour porter une probabilité et la propager à travers les opérateurs (MayBMS, MystiQ). Change `ferrite_common::Value` en profondeur ; à ne considérer que si un vrai cas d'usage (données de classifieur/capteur) apparaît, pas en spéculatif.

### Autres pistes (proposées ici, pas dans la liste initiale)

- **`EXPLAIN` réel avec le plan choisi** — le planificateur est à règles, pas à coûts, donc un `EXPLAIN` fidèle (quel access path retenu, index utilisé ou non) est peu coûteux à exposer maintenant et donne aux utilisateurs un moyen de comprendre/optimiser leurs requêtes sans attendre le reste de cette liste.
- **`current_identity()`** — équivalent de `current_user` côté Postgres, mais renvoyant une vraie `ferrite_common::Identity` exploitable dans une procédure/un trigger appelé depuis une requête, cohérent avec le modèle de sécurité déjà acté (§Modèle de sécurité).

### Côté lecture (mesuré après la passe JOIN/agrégats)

`sqlite_to_ferrite.py` dérive maintenant du schéma un `_after.sql` : le DDL
d'index, plus une requête par forme que toute application écrit. Rejouées
contre le vrai serveur, **15/15 sont acceptées, contre 6/15 avant cette
passe** — les 6 étant les `CREATE INDEX`, c'est-à-dire qu'aucune requête ne
passait. Et elles répondent, elles ne sont pas seulement acceptées :
`count(*)` sur `vr_room_objects` rend 394, exactement ce que rend SQLite ;
`ORDER BY created_at DESC LIMIT 20` rend ses 20 lignes ; le `JOIN` réel
`user_badges`/`users` et le `LEFT JOIN ... GROUP BY ... HAVING count(*) > 1`
rendent le même nombre de groupes que la requête équivalente sur la base
d'origine.

Une différence de sémantique à connaître avant de migrer : `LIKE` est
**sensible à la casse** dans Ferrite, comme dans PostgreSQL, alors qu'il est
insensible dans SQLite. Sur la même donnée, `WHERE name LIKE '%a%'` rend 136
lignes ici et 144 dans SQLite. `ILIKE` existe désormais et rend bien 144 :
c'est la cible de réécriture des trois recherches `LIKE` de PawChat, listées
dans `docs/pawchat-sql-audit.md`. `LIKE` n'a **pas** été rendu insensible,
la casse étant le bon comportement partout ailleurs.

~~Le risque le plus sérieux avant une mise en production n'est pas dialectal :
`ferrite-storage` n'ayant pas encore d'index secondaire, aucune contrainte
`PRIMARY KEY` ou `UNIQUE` n'est appliquée.~~ — *corrigé.* Les contraintes
d'unicité sont désormais **réellement appliquées** (`23505` sur violation),
sans index secondaire pour autant : c'est le moteur de stockage qui vérifie,
sous son verrou, juste avant l'écriture qu'il protège. Deux propriétés que
l'exécuteur ne pouvait pas se donner tout seul, et qui sont testées
séparément (`crates/ferrite-storage/tests/unique.rs`) :

- **la vérification voit plus large qu'un snapshot** — une ligne validée
  après la prise de snapshot, ou écrite par une transaction encore en vol,
  est invisible d'un `scan` et laissait passer le doublon ; l'unicité est
  une propriété de la table, pas d'une vue ;
- **vérification et écriture sont un seul pas** — vérifier puis écrire est
  un TOCTOU classique : deux transactions concurrentes ne trouvaient rien
  chacune de son côté et écrivaient toutes les deux.

Le coût d'un scan complet par écriture est évité par un cache négatif de
hachages de clés (`crates/ferrite-storage/src/unique.rs`) : un sur-ensemble
des clés vivantes, donc une absence y est une preuve d'absence et le scan
n'a lieu que sur suspicion de conflit. Sa justesse ne dépend que de
l'invariant de sur-ensemble, jamais de sa précision.

Limite assumée : une ligne en cours de suppression par une transaction
encore en vol est comptée comme présente. PostgreSQL attendrait cette
transaction ; sans gestionnaire de verrous pour attendre, refuser est la
seule réponse qui ne peut pas produire de doublon — trop stricte, jamais
fausse.

## Passe durcissement crash/corruption (août 2026)

Menée avec une seule question en tête : « impossible de corrompre les
données, aucun crash possible, et s'il y en a un, un redémarrage et tout
remarche ». Ce qui a été trouvé et corrigé, dans l'ordre de gravité :

1. **Aucune contrainte d'unicité n'était appliquée** — voir la section
   ci-dessus, corrigé et prouvé par le replay.
2. **Une chaîne `OR` plate abattait le process.** 192 termes,
   quelques kilo-octets de SQL : le parseur ne récursait pas (c'est une
   boucle), donc son garde de profondeur ne voyait rien, mais l'arbre
   produit gagnait un niveau par terme et tout le reste le parcourt
   récursivement. Un débordement de pile *abort* le process — ni
   `catch_unwind` ni l'isolation par tâche tokio ne le rattrapent. `AND`/`OR`
   sont désormais pliés en arbre équilibré, l'expansion de `IN (liste)`
   aussi.
3. **Un `CASE` imbriqué partait en exponentielle.** `infer()` inférait la
   première branche pour le type puis toutes les branches pour la
   nullabilité : `2^profondeur`. 32 niveaux et la connexion ne revenait
   jamais, en tenant un thread bloquant.
4. **Le journal grossissait sans borne.** Un seul replay du schéma PawChat
   laissait 13 Mio de base derrière **5,8 Gio** de journal, parce que rien
   ne le tronquait avant l'arrêt. Remplir le disque est un crash, et le
   seul dont la récupération ne sauve pas. `commit` checkpointe désormais
   au-delà de 64 Mio : quatre replays consécutifs tiennent maintenant dans
   ~40 Mio de journal au lieu de ~23 Gio.
5. **Une page corrompue indexait hors bornes.** Le checksum prouve que les
   octets sont ceux qui ont été écrits, pas qu'ils décrivent une page
   cohérente ; `slot_count`, `free_end` et l'étendue de chaque slot
   venaient du disque et indexaient le tampon plus loin. Validés à
   l'arrivée.
6. **Rien n'empêchait deux instances sur le même `FERRITE_DATA`.** Verrou
   exclusif tenu par le noyau sur un handle ouvert — donc relâché même si
   le process est tué, ce qu'un fichier-verrou ne fait pas.

7. **Le catalogue lui-même n'avait aucune clé unique appliquée.** Il est
   stocké dans des tables ordinaires, donc rien ne distinguait ses lignes
   des autres, et la seule garde contre deux lignes nommant une même table
   était une `HashMap` en mémoire privée à une instance de `SystemCatalog`
   — laquelle ne peut de toute façon en voir qu'une des deux, et se
   reconstruit depuis le stockage où le doublon gagne ou perd selon
   l'ordre d'itération. Le symptôme est déroutant plutôt que
   manifestement fatal : un `DROP TABLE` qui a l'air de marcher, puis un
   `CREATE TABLE` refusé en `42710`, parce que le drop a supprimé une
   ligne et le rechargement a trouvé l'autre. `ferrite_tables`,
   `ferrite_columns` et `ferrite_indexes` déclarent maintenant leurs clés
   et passent par la même application atomique que n'importe quelle table
   utilisateur. Cf. le rapport pour ce que ça ne prouve pas.

Plus : isolation d'un panic à la connexion fautive (avec un vrai
`ErrorResponse` plutôt qu'une socket qui tombe), timeout d'énoncé et de
transaction, borne sur le jeu de résultats matérialisé, plafond de
connexions simultanées.

## Reste à faire (pas encore scaffoldé, à trancher plus tard)

- Endpoint de métriques Prometheus — pas encore de crate/emplacement défini.
- `cargo-fuzz` : cibles à écrire une fois `ferrite-sql`/`ferrite-protocol` non-triviaux.
- Format exact du journal de récupération de `ferrite-storage` (WAL complet vs plus simple) — laissé au jugement de l'Agent 1 avec justification.
- Sous-ensemble exact de la grammaire SQL v1 (JOIN, CTE, quelles fonctions d'agrégat, quelles fenêtres) — laissé au jugement de l'Agent 2 avec justification, dans les limites du présent document.
