# ferrite-server — binaire serveur (voir crates/ferrite-server). Meme
# recette que chronotopedb/Dockerfile et Dockerfile.pawchat du fork
# SpacetimeDB de PawChat : cross-compile musl statique + strip + LTO fat
# pour une image finale minimale, cargo-chef pour ne recompiler les
# dependances que si Cargo.lock change.

ARG CARGO_STRIP=symbols
ARG CARGO_LTO=fat
ARG CARGO_CODEGEN_UNITS=1

FROM rust:1.93.0 AS chef
RUN rust_target=$(rustc -vV | awk '/^host:/{ print $2 }') && \
  curl https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-$rust_target.tgz -fL | tar xz -C $CARGO_HOME/bin
RUN cargo binstall -y cargo-chef@0.1.70
RUN rustup target add x86_64-unknown-linux-musl
RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
WORKDIR /usr/src/app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /usr/src/app/recipe.json .
ENV CARGO_INCREMENTAL=0
ARG CARGO_STRIP
ARG CARGO_LTO
ARG CARGO_CODEGEN_UNITS
ENV CARGO_PROFILE_RELEASE_STRIP=${CARGO_STRIP} \
    CARGO_PROFILE_RELEASE_LTO=${CARGO_LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CARGO_CODEGEN_UNITS}
RUN cargo chef cook --release -p ferrite-server --recipe-path recipe.json --target x86_64-unknown-linux-musl
COPY . .
RUN cargo build --release -p ferrite-server --locked --target x86_64-unknown-linux-musl

# ca-certificates : ferrite-server ne fait pas d'appel HTTPS sortant
# (serveur de protocole entrant uniquement), meme raisonnement que
# chronotopedb/Dockerfile et le Dockerfile.pawchat de SpacetimeDB — a
# reverifier une fois de vraies dependances reseau sortantes ajoutees
# (ex. si un mecanisme de licence/telemetry est introduit plus tard).
FROM alpine:3.20 AS runtime
RUN addgroup -S ferrite && adduser -S -G ferrite ferrite
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-musl/release/ferrite-server /usr/local/bin/

# Le repertoire de donnees est cree ici, possede par l'utilisateur non
# root du conteneur : un volume nomme monte sur un chemin vide herite des
# droits du repertoire de l'image, alors qu'un /data cree par le moteur
# Docker appartiendrait a root et le serveur echouerait au demarrage sur
# un EACCES. VOLUME le declare pour que `docker run` sans -v ne perde pas
# les donnees dans la couche ecrivable.
RUN mkdir -p /data && chown ferrite:ferrite /data
VOLUME ["/data"]

# 5432 : protocole PostgreSQL. 9187 : endpoint d'observabilite
# (/metrics + /health), le port de postgres_exporter. A garder sur un
# reseau interne : il n'est pas authentifie (voir crates/ferrite-metrics).
EXPOSE 5432 9187
ENV RUST_LOG=info \
    FERRITE_DATA=/data \
    FERRITE_METRICS_LISTEN=0.0.0.0:9187 \
    FERRITE_HEALTH_URL=http://127.0.0.1:9187/health
USER ferrite

# `nc -z 127.0.0.1 5432` ne prouvait que l'existence du listener. Un
# serveur dont le moteur est bloque — verrou de stockage tenu, disque
# plein, thread bloquant parti — accepte toujours la connexion TCP et
# passait donc ce test pendant que plus aucune requete n'aboutissait.
# /health fait un vrai aller-retour moteur (catalogue lu, transaction
# ouverte et validee, donc fsync du journal) sous une echeance de 3 s, et
# rend 503 avec la raison sinon.
#
# --retries=3 avec --interval=30s : le conteneur passe `unhealthy` apres
# ~90 s d'echecs consecutifs, pas au premier hoquet. Docker ne redemarre
# PAS sur un healthcheck rouge — c'est a l'orchestrateur de le faire
# (Swarm/Kubernetes le font ; avec `docker run --restart` seul, le
# healthcheck sert d'alerte et le redemarrage suit la mort du process).
# Voir la section « Production » du README.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -q -O /dev/null "$FERRITE_HEALTH_URL" || exit 1
ENTRYPOINT ["ferrite-server"]
