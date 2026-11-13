FROM rust:1.89-bookworm AS build
WORKDIR /build
# Tolerate slow or transient crates.io transfers instead of failing the build.
ENV CARGO_NET_RETRY=10 \
    CARGO_HTTP_TIMEOUT=60 \
    CARGO_HTTP_LOW_SPEED_LIMIT=1
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Cache the registry and the build directory across builds so a rebuild does not
# re-download or recompile every dependency. The binary is copied out of the
# cache mount because a cache mount is not part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release -p githearth-server -p githearth-cli \
    && cp target/release/githearth-server target/release/githearth /usr/local/bin/

# Git >= 2.40 is required: the rebase mergeability path uses
# `git merge-tree --merge-base`, added in 2.40. Debian trixie ships 2.47.
# openssh-client provides ssh-keygen, which `git verify-commit` needs for the
# signed-commit policy (gpg.format=ssh).
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git openssh-client && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home githearth && mkdir -p /var/lib/githearth && chown 10001:10001 /var/lib/githearth && chmod 0700 /var/lib/githearth
COPY --from=build /usr/local/bin/githearth-server /usr/local/bin/githearth-server
# The storage integrity checker used by the backup/restore and upgrade runbooks.
COPY --from=build /usr/local/bin/githearth /usr/local/bin/githearth
COPY LICENSE /usr/share/doc/githearth/LICENSE
COPY THIRD_PARTY_NOTICES /usr/share/doc/githearth/THIRD_PARTY_NOTICES
VOLUME ["/var/lib/githearth"]
ENV GITHEARTH_STORAGE_ROOT=/var/lib/githearth
ENV GITHEARTH_SSH_HOST_KEY_PATH=/var/lib/githearth/ssh_host_key
ENV GITHEARTH_SSH_BIND=0.0.0.0:2222
USER githearth
EXPOSE 8080 2222
ENTRYPOINT ["githearth-server"]
CMD ["serve"]
