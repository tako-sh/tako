FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8

RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  apt-get update \
  && apt-get install -y --no-install-recommends \
    openssh-client \
    curl \
    unzip \
    jq \
    ca-certificates \
    git \
    libvips42 \
  && rm -rf /var/lib/apt/lists/*

ENV BUN_INSTALL=/root/.bun
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  curl -fsSL https://bun.sh/install | bash \
  && ln -s "${BUN_INSTALL}/bin/bun" /usr/local/bin/bun

# Node.js + pnpm (for node fixtures)
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  curl -fsSL https://deb.nodesource.com/setup_24.x -o /tmp/nodesource.sh \
  && bash /tmp/nodesource.sh \
  && rm /tmp/nodesource.sh \
  && apt-get install -y --no-install-recommends nodejs \
  && npm install -g pnpm \
  && rm -rf /var/lib/apt/lists/*

# Go (for go fixtures)
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  ARCH="$(dpkg --print-architecture)" \
  && curl --fail --show-error --location --retry 5 --retry-all-errors --retry-delay 2 \
    --output /tmp/go.tar.gz \
    "https://dl.google.com/go/go1.26.5.linux-${ARCH}.tar.gz" \
  && tar -C /usr/local -xzf /tmp/go.tar.gz \
  && rm /tmp/go.tar.gz \
  && ln -s /usr/local/go/bin/go /usr/local/bin/go
ENV PATH="/usr/local/go/bin:$PATH"

WORKDIR /workspace

COPY e2e/docker/runner/run-e2e.sh /usr/local/bin/run-e2e.sh
RUN --mount=type=secret,id=proxy_ca,mode=0444 \
  if [ -s /run/secrets/proxy_ca ]; then \
    export SSL_CERT_FILE=/run/secrets/proxy_ca CURL_CA_BUNDLE=/run/secrets/proxy_ca NODE_EXTRA_CA_CERTS=/run/secrets/proxy_ca; \
    printf 'Acquire::https::CaInfo "/run/secrets/proxy_ca";\n' > /tmp/e2e-apt.conf; \
    export APT_CONFIG=/tmp/e2e-apt.conf; \
    trap 'rm -f /tmp/e2e-apt.conf' EXIT; \
  fi; \
  chmod +x /usr/local/bin/run-e2e.sh

ENTRYPOINT ["/usr/local/bin/run-e2e.sh"]
