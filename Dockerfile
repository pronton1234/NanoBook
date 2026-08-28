# Two stages so the shipped image carries a binary and nothing else: no
# toolchain, no source, no cargo registry. Smaller image, faster cold start,
# and a cold start is exactly what a free-tier host makes the visitor watch.
FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p server

FROM debian:bookworm-slim
# Unprivileged: the server binds a socket and runs benchmarks, and needs
# nothing else.
RUN useradd -m -u 10001 app
COPY --from=build /src/target/release/nanobook-server /usr/local/bin/
USER app
ENV PORT=8080
EXPOSE 8080
CMD ["nanobook-server"]
