# Craft Apps Host: one static binary that serves the apps, updates them and offers /admin.
# Built when the project is installed or upgraded; app updates never rebuild it.
FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked && strip target/release/craft-host

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/craft-host /usr/local/bin/craft-host
# Compose sets the real identity (user:) and umask (FILE_UMASK).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/craft-host"]
CMD ["serve"]
