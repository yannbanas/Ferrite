# Ferrite

Un clone de PostgreSQL en 100 % Rust — plus léger et plus rapide que Postgres en coupant délibérément les fonctionnalités à faible usage/forte complexité plutôt qu'en visant la parité complète.

Voir [`docs/architecture.md`](docs/architecture.md) pour le plan complet : ce qui est gardé, ce qui est coupé et pourquoi, le modèle de sécurité (identité + procédures plutôt qu'un DSL de policy RLS séparé), le découpage en crates, et l'ordre de dépendance entre elles.

## Structure

```
crates/
  ferrite-common/     types + traits partagés (aucune implémentation)
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
