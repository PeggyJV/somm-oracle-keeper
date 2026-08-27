# Build
FROM rust:1.96-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev ca-certificates protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 keeper
COPY --from=build /build/target/release/somm-oracle-keeper /usr/local/bin/
COPY keeper.toml /etc/keeper/keeper.toml
USER 10001
ENV KEEPER_CONFIG=/etc/keeper/keeper.toml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/somm-oracle-keeper"]
CMD ["run"]
