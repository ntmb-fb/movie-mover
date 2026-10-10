FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked

FROM scratch
COPY --from=build /app/target/release/movie-mover /movie-mover
# Listed so container UIs show the settings with their defaults.
ENV SRC=/Download/complete \
    DST=/film \
    TMDB_TOKEN="" \
    INTERVAL=30 \
    DRY_RUN="" \
    LOG_DIR=/data
ENTRYPOINT ["/movie-mover"]
