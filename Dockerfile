FROM rust:1.89-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p githearth-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home githearth
COPY --from=build /build/target/release/githearth-server /usr/local/bin/githearth-server
USER githearth
EXPOSE 8080
ENTRYPOINT ["githearth-server"]
CMD ["serve"]
