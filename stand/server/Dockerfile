FROM ghcr.io/astral-sh/uv:0.12.23 AS uv

FROM debian:bookworm-slim
ARG FACTORIO_VERSION=2.1.23
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/factorio.tar.xz "https://factorio.com/get-download/${FACTORIO_VERSION}/headless/linux64" \
 && tar -xJf /tmp/factorio.tar.xz -C /opt \
 && rm /tmp/factorio.tar.xz
COPY --from=uv /uv /usr/local/bin/uv
COPY entrypoint.sh /entrypoint.sh
COPY server-settings.json /config/server-settings.json
EXPOSE 34197/udp
ENTRYPOINT ["/bin/bash", "/entrypoint.sh"]
