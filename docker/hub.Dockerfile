FROM --platform=$BUILDPLATFORM rust:1.99.1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build
ARG TARGETARCH
WORKDIR /src
COPY companion/Cargo.toml companion/Cargo.lock ./
COPY companion/experiments/dtln-runtime/Cargo.toml ./experiments/dtln-runtime/Cargo.toml
# stub every cargo target so source edits retain the compiled dependency layer
RUN mkdir -p src/bin experiments/dtln-runtime/src \
 && touch src/lib.rs experiments/dtln-runtime/src/lib.rs \
 && printf 'fn main() {}\n' > src/main.rs \
 && cp src/main.rs src/bin/relay.rs \
 && cp src/main.rs src/bin/ptt-radio-hub.rs
# the hub has no native c dependencies; rust's bundled musl and lld cross-link it
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && rustup target add "$target" \
 && RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
    cargo build --locked --release --no-default-features --bin ptt-radio-hub --target "$target"

COPY companion/src/ ./src/
COPY companion/experiments/dtln-runtime/src/ ./experiments/dtln-runtime/src/
RUN find src experiments/dtln-runtime/src -type f -name '*.rs' -exec touch {} + \
 && case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && RUSTFLAGS="-C linker=rust-lld -C target-feature=+crt-static" \
    cargo build --locked --release --no-default-features --bin ptt-radio-hub --target "$target" \
 && mkdir /out \
 && cp "target/$target/release/ptt-radio-hub" /out/ptt-radio-hub

FROM scratch AS binary
COPY --from=build /out/ptt-radio-hub /ptt-radio-hub

FROM gcr.io/distroless/static-debian13:nonroot@sha256:18f4cc26c9ffa7f20a359298be7deadac5c607c8e03a02b8119f1a5fc2f82c99 AS runtime
ARG VERSION=0.4.0
LABEL org.opencontainers.image.source="https://github.com/vshuraeff/ptt-radio" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.version="$VERSION"
COPY --from=build /out/ptt-radio-hub /ptt-radio-hub
COPY LICENSE /LICENSE
USER 65532:65532
EXPOSE 34560/udp
ENTRYPOINT ["/ptt-radio-hub"]
