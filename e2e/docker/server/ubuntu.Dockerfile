FROM docker:28.4.0-dind@sha256:2ceb471176ad51e37145d43ce7cbf0fa5d644a2b185bd537f0ef695fb3a37497 AS docker

FROM ubuntu:26.04@sha256:6a06111c3b0a8600feadb270aec771dc239bf407b25625f93f44ab00e11f4d3c

ENV DEBIAN_FRONTEND=noninteractive

RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  set -eu; \
  sed -i 's|http://|https://|g' /etc/apt/sources.list.d/*.sources; \
  for attempt in 1 2 3 4 5; do \
    rm -rf /var/lib/apt/lists/*; \
    if apt-get -o Acquire::Retries=5 update \
      && apt-get install -y --no-install-recommends \
        openssh-server \
        bash \
        ca-certificates \
        curl \
        git \
        iptables \
        libvips42 \
        netcat-openbsd \
        npm \
        sudo \
        systemd systemd-sysv systemd-resolved dbus iproute2 wireguard-tools \
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


# The disposable host runs its own engine; never mount the executor's socket.
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker-proxy /usr/local/bin/docker-init /usr/local/bin/dockerd /usr/local/bin/containerd /usr/local/bin/containerd-shim-runc-v2 /usr/local/bin/runc /usr/local/bin/
COPY --from=docker /usr/local/libexec/docker/cli-plugins/ /usr/local/libexec/docker/cli-plugins/

# Pre-install tako-server (with dummy binary, installer creates user/service)
COPY scripts/install-tako-server.sh /tmp/install-tako-server.sh
COPY scripts/tako-network-helper.py /tmp/scripts/tako-network-helper.py
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  chmod +x /tmp/install-tako-server.sh \
    && ssh-keygen -A \
    && mkdir -p /run/sshd \
    && printf '#!/bin/sh\nexit 0\n' > /tmp/tako-server \
    && chmod +x /tmp/tako-server \
    && tar -cf - -C /tmp tako-server scripts/tako-network-helper.py | zstd -o /tmp/tako-server.tar.zst \
    && sha256sum /tmp/tako-server.tar.zst | awk '{print $1}' > /tmp/tako-server.tar.zst.sha256 \
    && TAKO_SERVER_URL="file:///tmp/tako-server.tar.zst" TAKO_RESTART_SERVICE=0 TAKO_SERVER_NAME=e2e sh /tmp/install-tako-server.sh \
    && rm -f /tmp/install-tako-server.sh /tmp/tako-server /tmp/tako-server.tar.zst /tmp/tako-server.tar.zst.sha256 /tmp/scripts/tako-network-helper.py

COPY e2e/docker/server/entrypoint.sh /usr/local/bin/tako-e2e-entrypoint.sh
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  chmod +x /usr/local/bin/tako-e2e-entrypoint.sh

EXPOSE 22

CMD ["/usr/local/bin/tako-e2e-entrypoint.sh"]
