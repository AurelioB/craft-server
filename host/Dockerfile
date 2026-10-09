# Craft Apps Host: build a static musl binary for the requested platform.
ARG VERSION=0.1.0
FROM rust:1.99-alpine AS build
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked && strip target/release/craft-host

# Export with buildx --target binary --output type=local,dest=<directory>.
FROM scratch AS binary
COPY --from=build /src/target/release/craft-host /craft-host

FROM gcr.io/distroless/static-debian12:nonroot
ARG VERSION
LABEL org.opencontainers.image.title="Craft Apps Host" \
      org.opencontainers.image.description="Self-hosted Craft browser apps and update service" \
      org.opencontainers.image.source="https://github.com/AurelioB/craft-server" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=build /src/target/release/craft-host /usr/local/bin/craft-host
# Compose sets the real identity (user:) and umask (FILE_UMASK).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/craft-host"]
CMD ["serve"]
