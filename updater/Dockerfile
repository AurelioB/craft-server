# Updater image: a static binary on a minimal runtime. Built once when the project is installed
# or upgraded; app updates never rebuild it.
FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && strip target/release/craft-updater

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/craft-updater /usr/local/bin/craft-updater
# The Compose file sets the real identity (user:) and umask (FILE_UMASK).
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/craft-updater"]
CMD ["daemon"]
