FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN set -eu; \
  for attempt in 1 2 3 4 5; do \
    rm -rf /var/lib/apt/lists/*; \
    if apt-get -o Acquire::Retries=5 update \
      && apt-get install -y --no-install-recommends \
        openssh-server \
        bash \
        ca-certificates \
        curl \
        git \
        netcat-openbsd \
        npm \
        sudo \
        xz-utils \
        zstd; then \
      rm -rf /var/lib/apt/lists/*; \
      exit 0; \
    fi; \
    if [ "$attempt" = "5" ]; then \
      exit 1; \
    fi; \
    sleep $((attempt * 10)); \
  done

# Pre-install tako-server (with dummy binary, installer creates user/service)
COPY scripts/install-tako-server.sh /tmp/install-tako-server.sh
RUN chmod +x /tmp/install-tako-server.sh \
    && printf '#!/bin/sh\nexit 0\n' > /tmp/tako-server \
    && chmod +x /tmp/tako-server \
    && tar -cf - -C /tmp tako-server | zstd -o /tmp/tako-server.tar.zst \
    && sha256sum /tmp/tako-server.tar.zst | awk '{print $1}' > /tmp/tako-server.tar.zst.sha256 \
    && TAKO_SERVER_URL="file:///tmp/tako-server.tar.zst" TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /tmp/install-tako-server.sh \
    && rm -f /tmp/install-tako-server.sh /tmp/tako-server /tmp/tako-server.tar.zst /tmp/tako-server.tar.zst.sha256

# Generate SSH host keys
RUN ssh-keygen -A

COPY e2e/docker/server/entrypoint.sh /usr/local/bin/tako-e2e-entrypoint.sh
RUN chmod +x /usr/local/bin/tako-e2e-entrypoint.sh

EXPOSE 22

CMD ["/usr/local/bin/tako-e2e-entrypoint.sh"]
