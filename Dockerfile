# FROM mcr.microsoft.com/cbl-mariner/base/rust:1 AS builder
FROM rust:alpine AS builder

COPY Cargo.lock /build/
COPY Cargo.toml /build/
COPY src /build/src

# Build the default page
WORKDIR /build

RUN apk add --no-cache --purge openssl-dev openssl-libs-static musl-dev libc-dev
RUN cargo build --release
RUN mkdir -p /app && mv target/release/symproxycloud /app/

FROM mcr.microsoft.com/cbl-mariner/distroless/minimal:2.0

COPY --from=builder /app /app
COPY default.toml /app/default.toml
# COPY static /app/static
# COPY templates /app/templates

#WORKDIR /home/site/wwwroot
WORKDIR /app
EXPOSE 8000

ENTRYPOINT [ "/app/symproxycloud" ]
