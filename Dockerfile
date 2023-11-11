##### Builder
FROM rust:1.73-slim as builder
ARG UID=65203
ARG GID=65203

RUN apt-get update && apt-get install musl-tools -y

RUN adduser                 \
    --disabled-password     \
    --gecos ""              \
    --home "/nonexistent"   \
    --shell "/sbin/nologin" \
    --no-create-home        \
    --uid "${UID}"          \
    --uid "${GID}"          \
    "zonefile"

RUN rustup target add x86_64-unknown-linux-musl

RUN mkdir -p /usr/src/zonefile
COPY . /usr/src/zonefile/

WORKDIR /usr/src/zonefile/

# Build it and copy the resulting binary into 
# /usr/local/bin since cache directories become
# inaccessible at the end of the running command.
RUN --mount=type=cache,target=/usr/local/cargo/registry         \
    --mount=type=cache,target=/usr/src/zonefile/target          \
    cargo build --target x86_64-unknown-linux-musl --release && \
    cp -r /usr/src/zonefile/target/x86_64-unknown-linux-musl/release/* /usr/local/bin/

FROM scratch AS zonefile
LABEL org.opencontainers.image.source=https://github.com/kubi-zone/zonefile
ARG UID
ARG GID
COPY --from=builder --chown=${UID}:${GID} --chmod=0440 /etc/passwd /etc/passwd
COPY --from=builder --chown=${UID}:${GID} --chmod=0440 /etc/group /etc/group
COPY --from=builder --chown=${UID}:${GID} --chmod=0550 /usr/local/bin/zonefile /app/zonefile
USER ${UID}:${GID}

ENTRYPOINT ["/app/zonefile"]
CMD ["print-crds"]
