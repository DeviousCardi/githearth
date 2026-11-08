FROM rust:1.89-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p githearth-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home githearth && mkdir -p /var/lib/githearth && chown 10001:10001 /var/lib/githearth && chmod 0700 /var/lib/githearth
COPY --from=build /build/target/release/githearth-server /usr/local/bin/githearth-server
COPY LICENSE /usr/share/doc/githearth/LICENSE
VOLUME ["/var/lib/githearth"]
ENV GITHEARTH_STORAGE_ROOT=/var/lib/githearth
ENV GITHEARTH_SSH_HOST_KEY_PATH=/var/lib/githearth/ssh_host_key
ENV GITHEARTH_SSH_BIND=0.0.0.0:2222
USER githearth
EXPOSE 8080 2222
ENTRYPOINT ["githearth-server"]
CMD ["serve"]
